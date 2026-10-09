//! In-process tests: project integration.

#[allow(unused_imports)]
use super::support::*;
use super::*;

#[test]
fn project_child_worktree_can_be_reviewed_fast_forwarded_and_cleaned_up() {
    let temp = workspace();
    let source = git_repository();
    let database_path = temp.join("state.sqlite");
    let backend = InProcessBackend::new_persistent(&database_path).unwrap();
    let connection = backend.connect();
    negotiate(&connection);
    let workspace_record = match connection
        .request(RequestEnvelope::new(ClientRequest::Workspace(
            WorkspaceRequest::CreateWorkspace {
                name: "Project worktree".to_owned(),
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
                workspace_id: workspace_record.id,
                name: "Manager".to_owned(),
            },
        )))
        .result
        .unwrap()
    {
        ServerResponse::Session(SessionResponse::AgentSessionCreated(session)) => session,
        response => panic!("unexpected session response: {response:?}"),
    };
    let parent_repository = match connection
        .request(RequestEnvelope::new(ClientRequest::Repository(
            RepositoryRequest::AttachSessionRepository {
                session_id: root.id,
                source: source.display().to_string(),
                path: "repo".to_owned(),
                revision: None,
                reuse_local: false,
            },
        )))
        .result
        .unwrap()
    {
        ServerResponse::Repository(RepositoryResponse::SessionRepositoryAttached(repository)) => {
            repository
        }
        response => panic!("unexpected repository response: {response:?}"),
    };
    let parent_git = connection
        .session_git(root.id, parent_repository.id)
        .unwrap();
    let base_revision = parent_git.status().unwrap().head.unwrap();

    let task_id = loom_core::TaskId::new();
    let child_session_id = AgentSessionId::new();
    let created_at = Timestamp::now();
    let child_snapshot = AgentSessionSnapshot {
        id: child_session_id,
        workspace_id: workspace_record.id,
        name: "Code child".to_owned(),
        state: AgentSessionState::Idle,
        created_at,
        updated_at: created_at,
    };
    let task = loom_core::DelegatedTaskRecord {
        task_id,
        project_id: ProjectId::from_uuid(*root.id.as_uuid()),
        requester_session_id: root.id,
        target_session_id: child_session_id,
        child_name: child_snapshot.name.clone(),
        intent: "Make a committed code change".to_owned(),
        model_id: "deterministic/demo".to_owned(),
        context_references: Vec::new(),
        dependencies: Vec::new(),
        code_change: true,
        permissions: loom_core::ProjectAgentPermissions::default(),
        status: loom_core::DelegatedTaskStatus::Queued,
        created_at,
        updated_at: created_at,
    };
    let mut worktree = ProjectWorktreeRecord {
        project_id: task.project_id,
        task_id,
        parent_session_id: root.id,
        child_session_id,
        parent_repository_id: parent_repository.id,
        child_repository_id: RepositoryId::new(),
        relative_path: format!("project-worktrees/{task_id}"),
        worktree_name: format!("loom-child-{task_id}"),
        branch_name: format!("loom/project-child-{task_id}"),
        base_revision,
        result_revision: None,
        integrated_revision: None,
        status: ProjectWorktreeStatus::Creating,
        conflict_paths: Vec::new(),
        error: None,
        cleanup_disposition: None,
        created_at,
        updated_at: created_at,
    };
    backend
        .persistence
        .as_ref()
        .unwrap()
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
        .create_in_workspace_with_id(
            workspace_record.id,
            child_session_id,
            child_snapshot.name.clone(),
        )
        .unwrap();
    backend.journal().unwrap().append_session(event);
    connection
        .ensure_project_worktree_ready(&mut worktree)
        .unwrap();

    let child_checkout = connection
        .session_filesystem(child_session_id)
        .unwrap()
        .root()
        .join(&worktree.relative_path);
    fs::write(child_checkout.join("README.md"), "child result\n").unwrap();
    let git = |arguments: &[&str]| {
        let output = Command::new("git")
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_INDEX_FILE")
            .env_remove("GIT_COMMON_DIR")
            .args(["-C", child_checkout.to_str().unwrap()])
            .args(arguments)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {:?} failed: {}",
            arguments,
            String::from_utf8_lossy(&output.stderr)
        );
    };
    git(&["config", "user.name", "Loom Test"]);
    git(&["config", "user.email", "loom@example.test"]);
    git(&["add", "--", "README.md"]);
    git(&["commit", "-qm", "child result"]);
    backend
        .persistence
        .as_ref()
        .unwrap()
        .update_delegated_task_status(
            task_id,
            loom_core::DelegatedTaskStatus::Completed,
            Timestamp::now(),
        )
        .unwrap();

    let integration_before_review = connection.request(RequestEnvelope::new(
        ClientRequest::Project(ProjectRequest::IntegrateProjectChild {
            project_id: task.project_id,
            manager_session_id: root.id,
            task_id,
            expected_parent_revision: worktree.base_revision.clone(),
        }),
    ));
    assert!(matches!(
        integration_before_review.result,
        Err(error) if error.code == ErrorCode::InvalidState
    ));

    let review = connection.request(RequestEnvelope::new(ClientRequest::Project(
        ProjectRequest::GetProjectChildReview {
            project_id: task.project_id,
            manager_session_id: root.id,
            task_id,
        },
    )));
    let ServerResponse::Project(ProjectResponse::ProjectChildReview {
        worktree: reviewed,
        status: reviewed_status,
        diff,
    }) = review.result.unwrap()
    else {
        panic!("unexpected child review response");
    };
    assert_eq!(reviewed.status, ProjectWorktreeStatus::Ready);
    assert_eq!(
        reviewed_status.branch.as_deref(),
        Some(worktree.branch_name.as_str())
    );
    assert!(diff.patch.contains("child result"));

    let completion_guard = ProjectAgentTools {
        backend: Arc::downgrade(&backend),
        session_id: root.id,
        project_id: task.project_id,
        model_id: ModelId::new("deterministic/demo"),
        can_delegate: false,
        can_delegate_code: false,
        can_message: false,
        can_branch_message: false,
        can_inspect_children: false,
        can_wait_children: false,
        can_control_children: false,
        can_review_children: false,
        can_integrate_children: false,
    };
    let unreviewed_path = child_checkout.join("unreviewed.txt");
    fs::write(&unreviewed_path, "unreviewed change\n").unwrap();
    assert!(
        completion_guard
            .completion_blocker()
            .is_some_and(|blocker| blocker.contains("changed after review"))
    );
    fs::remove_file(unreviewed_path).unwrap();

    let integration = connection.request(RequestEnvelope::new(ClientRequest::Project(
        ProjectRequest::IntegrateProjectChild {
            project_id: task.project_id,
            manager_session_id: root.id,
            task_id,
            expected_parent_revision: worktree.base_revision.clone(),
        },
    )));
    let ServerResponse::Project(ProjectResponse::ProjectChildWorktreeUpdated(integrated)) =
        integration.result.unwrap()
    else {
        panic!("unexpected child integration response");
    };
    assert_eq!(integrated.status, ProjectWorktreeStatus::Integrated);
    assert_eq!(
        parent_git.status().unwrap().head,
        integrated.integrated_revision
    );
    assert_eq!(completion_guard.completion_blocker(), None);

    let cleanup = connection.request(RequestEnvelope::new(ClientRequest::Project(
        ProjectRequest::CleanupProjectChildWorktree {
            project_id: task.project_id,
            manager_session_id: root.id,
            task_id,
            disposition: ProjectWorktreeCleanupDisposition::RemoveClean,
        },
    )));
    let ServerResponse::Project(ProjectResponse::ProjectChildWorktreeUpdated(removed)) =
        cleanup.result.unwrap()
    else {
        panic!("unexpected child cleanup response");
    };
    assert_eq!(removed.status, ProjectWorktreeStatus::Removed);
    assert!(!child_checkout.exists());
    assert_eq!(
        fs::read_to_string(parent_git.root().join("README.md")).unwrap(),
        "child result\n"
    );

    drop(connection);
    backend.shutdown().unwrap();
    drop(backend);
    fs::remove_dir_all(temp).unwrap();
    fs::remove_dir_all(source).unwrap();
}

#[test]
fn project_child_integrates_a_merge_over_an_advanced_parent() {
    let fixture = project_code_child_fixture();
    commit_file(
        &fixture.child_checkout,
        "child.txt",
        "child\n",
        "child change",
    );
    complete_project_code_child(&fixture);
    let child_revision = review_project_code_child(&fixture);

    commit_file(
        fixture.parent_git.root(),
        "README.md",
        "parent advanced\n",
        "parent change",
    );
    let parent_advanced = fixture.parent_git.status().unwrap().head.unwrap();

    let integration = fixture
        .connection
        .request(RequestEnvelope::new(ClientRequest::Project(
            ProjectRequest::IntegrateProjectChild {
                project_id: fixture.project_id,
                manager_session_id: fixture.root_id,
                task_id: fixture.task_id,
                expected_parent_revision: fixture.worktree.base_revision.clone(),
            },
        )));
    let ServerResponse::Project(ProjectResponse::ProjectChildWorktreeUpdated(integrated)) =
        integration.result.unwrap()
    else {
        panic!("unexpected child integration response");
    };
    assert_eq!(integrated.status, ProjectWorktreeStatus::Integrated);
    let integrated_revision = integrated.integrated_revision.clone().unwrap();
    assert_ne!(integrated_revision, parent_advanced);
    assert_ne!(integrated_revision, child_revision);
    assert_eq!(
        fixture.parent_git.status().unwrap().head.as_deref(),
        Some(integrated_revision.as_str())
    );
    assert_eq!(
        fs::read_to_string(fixture.parent_git.root().join("README.md")).unwrap(),
        "parent advanced\n"
    );
    assert_eq!(
        fs::read_to_string(fixture.parent_git.root().join("child.txt")).unwrap(),
        "child\n"
    );
    let parents = Command::new("git")
        .args([
            "-C",
            fixture.parent_git.root().to_str().unwrap(),
            "rev-list",
            "--parents",
            "-n1",
            &integrated_revision,
        ])
        .output()
        .unwrap();
    assert!(parents.status.success());
    let parents = String::from_utf8(parents.stdout).unwrap();
    let parents: Vec<&str> = parents.split_whitespace().collect();
    assert_eq!(parents.len(), 3, "expected a two-parent merge commit");
    assert!(parents.contains(&parent_advanced.as_str()));
    assert!(parents.contains(&child_revision.as_str()));

    // The guard's intent is that the reviewed result is contained in what was
    // integrated. A merge records the merge commit rather than the child
    // revision, so a merge integration must still satisfy `completion_blocker`.
    let completion_guard = ProjectAgentTools {
        backend: Arc::downgrade(&fixture.backend),
        session_id: fixture.root_id,
        project_id: fixture.project_id,
        model_id: ModelId::new("deterministic/demo"),
        can_delegate: false,
        can_delegate_code: false,
        can_message: false,
        can_branch_message: false,
        can_inspect_children: false,
        can_wait_children: false,
        can_control_children: false,
        can_review_children: false,
        can_integrate_children: false,
    };
    assert_eq!(completion_guard.completion_blocker(), None);
    fixture.shutdown();
}

#[test]
fn project_child_integration_conflict_is_refused_and_preserved() {
    let fixture = project_code_child_fixture();
    commit_file(
        &fixture.child_checkout,
        "README.md",
        "child result\n",
        "child change",
    );
    complete_project_code_child(&fixture);
    let child_revision = review_project_code_child(&fixture);

    commit_file(
        fixture.parent_git.root(),
        "README.md",
        "parent version\n",
        "parent change",
    );
    let parent_advanced = fixture.parent_git.status().unwrap().head.unwrap();

    let integration = fixture
        .connection
        .request(RequestEnvelope::new(ClientRequest::Project(
            ProjectRequest::IntegrateProjectChild {
                project_id: fixture.project_id,
                manager_session_id: fixture.root_id,
                task_id: fixture.task_id,
                expected_parent_revision: fixture.worktree.base_revision.clone(),
            },
        )));
    let Err(error) = integration.result else {
        panic!("conflicting integration should be refused");
    };
    assert_eq!(error.code, ErrorCode::Conflict);
    assert!(error.message.contains("README.md"));

    let worktree = integrated_worktree(&fixture);
    assert_eq!(worktree.status, ProjectWorktreeStatus::Conflict);
    assert_eq!(worktree.conflict_paths, vec!["README.md".to_owned()]);
    assert_eq!(
        fixture.parent_git.status().unwrap().head.as_deref(),
        Some(parent_advanced.as_str())
    );
    assert!(fixture.parent_git.status().unwrap().clean);
    assert_eq!(
        fs::read_to_string(fixture.parent_git.root().join("README.md")).unwrap(),
        "parent version\n"
    );
    assert_eq!(
        fs::read_to_string(fixture.child_checkout.join("README.md")).unwrap(),
        "child result\n"
    );
    let child_head = Command::new("git")
        .args([
            "-C",
            fixture.child_checkout.to_str().unwrap(),
            "rev-parse",
            "HEAD",
        ])
        .output()
        .unwrap();
    assert!(child_head.status.success());
    assert_eq!(
        String::from_utf8(child_head.stdout).unwrap().trim(),
        child_revision
    );
    fixture.shutdown();
}

#[test]
fn project_child_integration_rejects_an_unknown_expected_parent_revision() {
    let fixture = project_code_child_fixture();
    commit_file(
        &fixture.child_checkout,
        "child.txt",
        "child\n",
        "child change",
    );
    complete_project_code_child(&fixture);
    let child_revision = review_project_code_child(&fixture);

    let integration = fixture
        .connection
        .request(RequestEnvelope::new(ClientRequest::Project(
            ProjectRequest::IntegrateProjectChild {
                project_id: fixture.project_id,
                manager_session_id: fixture.root_id,
                task_id: fixture.task_id,
                expected_parent_revision: child_revision,
            },
        )));
    let Err(error) = integration.result else {
        panic!("integration onto an unrelated expected revision should be refused");
    };
    assert_eq!(error.code, ErrorCode::Conflict);
    assert!(error.message.contains("does not descend"));
    assert_eq!(
        integrated_worktree(&fixture).status,
        ProjectWorktreeStatus::Ready
    );
    assert_eq!(
        fixture.parent_git.status().unwrap().head.as_deref(),
        Some(fixture.worktree.base_revision.as_str())
    );
    fixture.shutdown();
}

#[test]
fn project_child_integration_surfaces_an_in_progress_git_operation() {
    let fixture = project_code_child_fixture();
    commit_file(
        &fixture.child_checkout,
        "child.txt",
        "child\n",
        "child change",
    );
    complete_project_code_child(&fixture);
    let child_revision = review_project_code_child(&fixture);
    fs::write(
        fixture.parent_git.root().join(".git").join("MERGE_HEAD"),
        format!("{child_revision}\n"),
    )
    .unwrap();

    let integration = fixture
        .connection
        .request(RequestEnvelope::new(ClientRequest::Project(
            ProjectRequest::IntegrateProjectChild {
                project_id: fixture.project_id,
                manager_session_id: fixture.root_id,
                task_id: fixture.task_id,
                expected_parent_revision: fixture.worktree.base_revision.clone(),
            },
        )));
    let Err(error) = integration.result else {
        panic!("integration during an in-progress Git operation should be refused");
    };
    assert_eq!(error.code, ErrorCode::RecoveryRequired);
    let worktree = integrated_worktree(&fixture);
    assert_eq!(worktree.status, ProjectWorktreeStatus::RecoveryRequired);
    assert!(
        worktree
            .error
            .as_deref()
            .is_some_and(|error| error.contains("in-progress Git operation"))
    );

    // Restart recovery surfaces the same state instead of replaying the merge.
    fixture
        .connection
        .reconcile_project_child_integrations(fixture.project_id)
        .unwrap();
    assert_eq!(
        integrated_worktree(&fixture).status,
        ProjectWorktreeStatus::RecoveryRequired
    );
    fixture.shutdown();
}

#[test]
fn interrupted_project_child_integration_recovers_without_replay() {
    let fixture = project_code_child_fixture();
    commit_file(
        &fixture.child_checkout,
        "child.txt",
        "child\n",
        "child change",
    );
    complete_project_code_child(&fixture);
    let child_revision = review_project_code_child(&fixture);

    commit_file(
        fixture.parent_git.root(),
        "README.md",
        "parent advanced\n",
        "parent change",
    );
    let parent_advanced = fixture.parent_git.status().unwrap().head.unwrap();
    fixture
        .parent_git
        .integrate_merge_revisions(
            &parent_advanced,
            &child_revision,
            "loom: integrate project child",
        )
        .unwrap();
    let merged_revision = fixture.parent_git.status().unwrap().head.unwrap();
    assert_ne!(merged_revision, parent_advanced);

    // Simulate a crash after the merge landed but before the durable record
    // was updated to `integrated`.
    let mut interrupted = integrated_worktree(&fixture);
    interrupted.status = ProjectWorktreeStatus::Integrating;
    interrupted.result_revision = Some(child_revision.clone());
    fixture
        .connection
        .save_project_worktree_state(&interrupted)
        .unwrap();

    fixture
        .connection
        .reconcile_project_child_integrations(fixture.project_id)
        .unwrap();
    let recovered = integrated_worktree(&fixture);
    assert_eq!(recovered.status, ProjectWorktreeStatus::Integrated);
    assert_eq!(
        recovered.integrated_revision.as_deref(),
        Some(merged_revision.as_str())
    );
    assert_eq!(
        fixture.parent_git.status().unwrap().head.as_deref(),
        Some(merged_revision.as_str())
    );

    // Re-running recovery is idempotent and does not replay the merge.
    fixture
        .connection
        .reconcile_project_child_integrations(fixture.project_id)
        .unwrap();
    assert_eq!(
        fixture.parent_git.status().unwrap().head.as_deref(),
        Some(merged_revision.as_str())
    );
    fixture.shutdown();
}

#[test]
fn nested_code_child_worktree_integrates_through_parent_to_root() {
    let temp = workspace();
    let source = git_repository();
    let persistence_path = temp.join("state.sqlite");
    let mut model = ScriptedOpenAiEndpoint::start();
    let model_endpoint = model.endpoint.clone();
    let model_id = ModelId::new("fixture/nested-worktree-integration");
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
                name: "Nested project worktree integration".to_owned(),
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
                name: "Root manager".to_owned(),
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
    let root_repository = match connection
        .request(RequestEnvelope::new(ClientRequest::Repository(
            RepositoryRequest::AttachSessionRepository {
                session_id: root,
                source: source.display().to_string(),
                path: "repo".to_owned(),
                revision: None,
                reuse_local: false,
            },
        )))
        .result
        .unwrap()
    {
        ServerResponse::Repository(RepositoryResponse::SessionRepositoryAttached(repository)) => {
            repository
        }
        response => panic!("unexpected repository response: {response:?}"),
    };
    let root_git = connection.session_git(root, root_repository.id).unwrap();
    let root_base_revision = root_git.status().unwrap().head.unwrap();

    let manager_permissions = loom_core::ProjectAgentPermissions {
        delegation: true,
        worktree_creation: true,
        review: true,
        integration: true,
        ..loom_core::ProjectAgentPermissions::default()
    };
    let (manager_task, manager_agent) = match connection
        .create_project_child(
            RequestId::new(),
            root,
            "code-manager".to_owned(),
            loom_core::DelegatedTaskSpec {
                intent: "Integrate a reviewed nested code change.".to_owned(),
                model_id: model_id.as_str().to_owned(),
                context_references: vec![],
                dependencies: vec![],
                code_change: true,
                permissions: manager_permissions,
            },
        )
        .unwrap()
    {
        ServerResponse::Project(ProjectResponse::ProjectChildCreated { task, child }) => {
            (task, child)
        }
        response => panic!("unexpected manager creation response: {response:?}"),
    };
    assert_eq!(manager_task.status, loom_core::DelegatedTaskStatus::Running);
    assert!(manager_task.code_change);
    assert_eq!(manager_task.permissions, manager_permissions);
    let manager_session = manager_agent.session_id;
    let project_id = manager_task.project_id;
    let persistence = backend.persistence.as_ref().unwrap();
    let manager_worktree = persistence
        .load_project_worktree_by_task(manager_task.task_id)
        .unwrap()
        .expect("code manager should have a durable worktree");
    assert_eq!(manager_worktree.status, ProjectWorktreeStatus::Ready);
    assert_eq!(manager_worktree.parent_session_id, root);
    assert_eq!(manager_worktree.parent_repository_id, root_repository.id);
    assert_eq!(manager_worktree.base_revision, root_base_revision);
    let manager_git = connection
        .session_git(manager_session, manager_worktree.child_repository_id)
        .unwrap();
    assert_eq!(
        manager_git.status().unwrap().head.as_deref(),
        Some(root_base_revision.as_str())
    );

    // Keep the manager's only workspace slot occupied while the nested
    // child is created, reviewed, and integrated into its checkout.
    let manager_turn = model.next_for_child();
    assert!(request_has_tool(
        &manager_turn.request,
        "delegate_project_task"
    ));
    assert!(request_has_tool(
        &manager_turn.request,
        "review_project_child"
    ));
    assert!(request_has_tool(
        &manager_turn.request,
        "integrate_project_child"
    ));

    let (grandchild_task, grandchild_agent) = match connection
        .create_project_child(
            RequestId::new(),
            manager_session,
            "nested-code-child".to_owned(),
            loom_core::DelegatedTaskSpec {
                intent: "Commit the nested result for integration.".to_owned(),
                model_id: model_id.as_str().to_owned(),
                context_references: vec![],
                dependencies: vec![],
                code_change: true,
                permissions: loom_core::ProjectAgentPermissions::default(),
            },
        )
        .unwrap()
    {
        ServerResponse::Project(ProjectResponse::ProjectChildCreated { task, child }) => {
            (task, child)
        }
        response => panic!("unexpected nested child creation response: {response:?}"),
    };
    assert_eq!(
        grandchild_task.status,
        loom_core::DelegatedTaskStatus::Queued
    );
    assert_eq!(grandchild_task.requester_session_id, manager_session);
    let grandchild_worktree = persistence
        .load_project_worktree_by_task(grandchild_task.task_id)
        .unwrap()
        .expect("nested code child should have a durable worktree");
    assert_eq!(grandchild_worktree.status, ProjectWorktreeStatus::Ready);
    assert_eq!(grandchild_worktree.parent_session_id, manager_session);
    assert_eq!(
        grandchild_worktree.parent_repository_id,
        manager_worktree.child_repository_id
    );
    assert_eq!(
        grandchild_worktree.base_revision, root_base_revision,
        "nested worktree should branch from the manager checkout HEAD"
    );
    let grandchild_checkout = connection
        .session_filesystem(grandchild_agent.session_id)
        .unwrap()
        .root()
        .join(&grandchild_worktree.relative_path);
    fs::write(
        grandchild_checkout.join("nested-result.txt"),
        "grandchild result\n",
    )
    .unwrap();
    let git = |arguments: &[&str]| {
        let output = Command::new("git")
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_INDEX_FILE")
            .env_remove("GIT_COMMON_DIR")
            .args(["-C", grandchild_checkout.to_str().unwrap()])
            .args(arguments)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {:?} failed: {}",
            arguments,
            String::from_utf8_lossy(&output.stderr)
        );
    };
    git(&["config", "user.name", "Loom Test"]);
    git(&["config", "user.email", "loom@example.test"]);
    git(&["add", "--", "nested-result.txt"]);
    git(&["commit", "-qm", "nested result"]);
    let grandchild_git = connection
        .session_git(
            grandchild_agent.session_id,
            grandchild_worktree.child_repository_id,
        )
        .unwrap();
    let grandchild_revision = grandchild_git.status().unwrap().head.unwrap();
    assert_ne!(grandchild_revision, grandchild_worktree.base_revision);
    persistence
        .update_delegated_task_status(
            grandchild_task.task_id,
            loom_core::DelegatedTaskStatus::Completed,
            Timestamp::now(),
        )
        .unwrap();

    let review = connection.request(RequestEnvelope::new(ClientRequest::Project(
        ProjectRequest::GetProjectChildReview {
            project_id,
            manager_session_id: manager_session,
            task_id: grandchild_task.task_id,
        },
    )));
    let ServerResponse::Project(ProjectResponse::ProjectChildReview {
        worktree: reviewed_grandchild,
        status: grandchild_status,
        diff,
    }) = review.result.unwrap()
    else {
        panic!("unexpected nested child review response");
    };
    assert_eq!(reviewed_grandchild.status, ProjectWorktreeStatus::Ready);
    assert_eq!(
        grandchild_status.head.as_deref(),
        Some(grandchild_revision.as_str())
    );
    assert!(diff.patch.contains("grandchild result"));
    assert_eq!(
        root_git.status().unwrap().head.as_deref(),
        Some(root_base_revision.as_str()),
        "reviewing the grandchild must not advance the root checkout"
    );
    assert!(
        !root_git.root().join("nested-result.txt").exists(),
        "the nested change should not reach root before either integration"
    );

    let integration = connection.request(RequestEnvelope::new(ClientRequest::Project(
        ProjectRequest::IntegrateProjectChild {
            project_id,
            manager_session_id: manager_session,
            task_id: grandchild_task.task_id,
            expected_parent_revision: grandchild_worktree.base_revision.clone(),
        },
    )));
    let ServerResponse::Project(ProjectResponse::ProjectChildWorktreeUpdated(
        integrated_grandchild,
    )) = integration.result.unwrap()
    else {
        panic!("unexpected nested child integration response");
    };
    assert_eq!(
        integrated_grandchild.status,
        ProjectWorktreeStatus::Integrated
    );
    assert_eq!(
        integrated_grandchild.integrated_revision.as_deref(),
        Some(grandchild_revision.as_str())
    );
    assert_eq!(
        manager_git.status().unwrap().head.as_deref(),
        Some(grandchild_revision.as_str()),
        "integrating the grandchild should advance the manager checkout"
    );
    let manager_checkout = connection
        .session_filesystem(manager_session)
        .unwrap()
        .root()
        .join(&manager_worktree.relative_path);
    assert_eq!(
        fs::read_to_string(manager_checkout.join("nested-result.txt")).unwrap(),
        "grandchild result\n"
    );
    assert_eq!(
        root_git.status().unwrap().head.as_deref(),
        Some(root_base_revision.as_str()),
        "the grandchild integration should stop at its manager parent"
    );

    model.respond_with_text(
        manager_turn,
        "The nested result is integrated and the delegated work is complete.",
    );
    let manager_run_id = persistence
        .load_latest_run_summary_for_session(manager_session)
        .unwrap()
        .expect("manager run should have a durable checkpoint")
        .snapshot
        .id;
    assert_eq!(
        await_settled_run(&connection, manager_run_id).state,
        AgentRunState::Completed
    );
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let status = persistence
            .load_delegated_task(manager_task.task_id)
            .unwrap()
            .unwrap()
            .status;
        if status == loom_core::DelegatedTaskStatus::Completed {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "manager should complete after its child is integrated; status={status:?}"
        );
        thread::sleep(Duration::from_millis(1));
    }

    let root_review = connection.request(RequestEnvelope::new(ClientRequest::Project(
        ProjectRequest::GetProjectChildReview {
            project_id,
            manager_session_id: root,
            task_id: manager_task.task_id,
        },
    )));
    let ServerResponse::Project(ProjectResponse::ProjectChildReview {
        worktree: reviewed_manager,
        status: manager_status,
        diff: manager_diff,
    }) = root_review.result.unwrap()
    else {
        panic!("unexpected manager review response");
    };
    assert_eq!(reviewed_manager.status, ProjectWorktreeStatus::Ready);
    assert_eq!(
        manager_status.head.as_deref(),
        Some(grandchild_revision.as_str())
    );
    assert!(manager_diff.patch.contains("grandchild result"));
    assert_eq!(
        reviewed_manager.base_revision, root_base_revision,
        "manager worktree should retain its original root-relative base"
    );

    let root_integration = connection.request(RequestEnvelope::new(ClientRequest::Project(
        ProjectRequest::IntegrateProjectChild {
            project_id,
            manager_session_id: root,
            task_id: manager_task.task_id,
            expected_parent_revision: manager_worktree.base_revision.clone(),
        },
    )));
    let ServerResponse::Project(ProjectResponse::ProjectChildWorktreeUpdated(integrated_manager)) =
        root_integration.result.unwrap()
    else {
        panic!("unexpected manager integration response");
    };
    assert_eq!(integrated_manager.status, ProjectWorktreeStatus::Integrated);
    assert_eq!(
        integrated_manager.integrated_revision.as_deref(),
        Some(grandchild_revision.as_str())
    );
    assert_eq!(
        root_git.status().unwrap().head.as_deref(),
        Some(grandchild_revision.as_str()),
        "integrating the manager should advance root to the nested result"
    );
    assert_eq!(
        fs::read_to_string(root_git.root().join("nested-result.txt")).unwrap(),
        "grandchild result\n"
    );

    drop(connection);
    backend.shutdown().unwrap();
    drop(backend);
    drop(model);
    fs::remove_dir_all(temp).unwrap();
    fs::remove_dir_all(source).unwrap();
}

#[test]
fn read_only_child_receives_an_isolated_checkout() {
    let temp = workspace();
    let source = git_repository();
    let database_path = temp.join("state.sqlite");
    let backend = InProcessBackend::new_persistent(&database_path).unwrap();
    let connection = backend.connect();
    negotiate(&connection);
    let workspace_record = match connection
        .request(RequestEnvelope::new(ClientRequest::Workspace(
            WorkspaceRequest::CreateWorkspace {
                name: "Read-only checkout".to_owned(),
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
                workspace_id: workspace_record.id,
                name: "Manager".to_owned(),
            },
        )))
        .result
        .unwrap()
    {
        ServerResponse::Session(SessionResponse::AgentSessionCreated(session)) => session,
        response => panic!("unexpected session response: {response:?}"),
    };
    let parent_repository = match connection
        .request(RequestEnvelope::new(ClientRequest::Repository(
            RepositoryRequest::AttachSessionRepository {
                session_id: root.id,
                source: source.display().to_string(),
                path: "repo".to_owned(),
                revision: None,
                reuse_local: false,
            },
        )))
        .result
        .unwrap()
    {
        ServerResponse::Repository(RepositoryResponse::SessionRepositoryAttached(repository)) => {
            repository
        }
        response => panic!("unexpected repository response: {response:?}"),
    };
    let base_revision = connection
        .session_git(root.id, parent_repository.id)
        .unwrap()
        .status()
        .unwrap()
        .head
        .unwrap();
    let project_id = ProjectId::from_uuid(*root.id.as_uuid());

    // A non-code task that only needs to read the repository now receives an
    // isolated checkout instead of forcing the manager to paste file contents.
    let (task, child) = match connection
        .create_project_child(
            RequestId::new(),
            root.id,
            "Reader".to_owned(),
            loom_core::DelegatedTaskSpec {
                intent: "Read the README and summarize it.".to_owned(),
                model_id: "deterministic/demo".to_owned(),
                context_references: vec![],
                dependencies: vec![],
                code_change: false,
                permissions: loom_core::ProjectAgentPermissions::default(),
            },
        )
        .unwrap()
    {
        ServerResponse::Project(ProjectResponse::ProjectChildCreated { task, child }) => {
            (task, child)
        }
        response => panic!("unexpected child response: {response:?}"),
    };
    assert!(!task.code_change);

    let persistence = backend.persistence.as_ref().unwrap();
    let mut worktree = persistence
        .load_project_worktree_by_task(task.task_id)
        .unwrap()
        .expect("a read-only child should receive an isolated checkout");
    connection
        .ensure_project_worktree_ready(&mut worktree)
        .unwrap();
    assert_eq!(worktree.status, ProjectWorktreeStatus::Ready);
    assert_eq!(worktree.base_revision, base_revision);

    // The checkout is the child's own copy, not the parent checkout, and it is
    // registered as the child session's repository.
    let child_checkout = connection
        .session_filesystem(child.session_id)
        .unwrap()
        .root()
        .join(&worktree.relative_path);
    assert!(child_checkout.join("README.md").is_file());
    assert_eq!(
        backend
            .session_repositories()
            .unwrap()
            .get(&child.session_id)
            .and_then(|repositories| repositories.get(&worktree.child_repository_id))
            .map(|repository| repository.path.clone()),
        Some(worktree.relative_path.clone())
    );

    // A read-only child has nothing to review or integrate.
    let review = connection.request(RequestEnvelope::new(ClientRequest::Project(
        ProjectRequest::GetProjectChildReview {
            project_id,
            manager_session_id: root.id,
            task_id: task.task_id,
        },
    )));
    assert!(review.result.is_err());
    let integrate = connection.request(RequestEnvelope::new(ClientRequest::Project(
        ProjectRequest::IntegrateProjectChild {
            project_id,
            manager_session_id: root.id,
            task_id: task.task_id,
            expected_parent_revision: base_revision.clone(),
        },
    )));
    assert!(integrate.result.is_err());

    // Its checkout can still be cleaned up once the task is terminal.
    assert!(
        persistence
            .update_delegated_task_status(
                task.task_id,
                loom_core::DelegatedTaskStatus::Completed,
                Timestamp::now(),
            )
            .unwrap()
    );
    let cleanup = connection.request(RequestEnvelope::new(ClientRequest::Project(
        ProjectRequest::CleanupProjectChildWorktree {
            project_id,
            manager_session_id: root.id,
            task_id: task.task_id,
            disposition: ProjectWorktreeCleanupDisposition::RemoveClean,
        },
    )));
    assert!(matches!(
        cleanup.result,
        Ok(ServerResponse::Project(
            ProjectResponse::ProjectChildWorktreeUpdated(worktree)
        )) if worktree.status == ProjectWorktreeStatus::Removed
    ));
    assert!(
        backend
            .session_repositories()
            .unwrap()
            .get(&child.session_id)
            .is_none_or(|repositories| repositories.is_empty())
    );

    drop(connection);
    backend.shutdown().unwrap();
    drop(backend);
    fs::remove_dir_all(temp).unwrap();
    fs::remove_dir_all(source).unwrap();
}
