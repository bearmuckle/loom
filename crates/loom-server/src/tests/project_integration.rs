//! In-process tests: project integration.

#[allow(unused_imports)]
use super::support::*;
use super::*;

#[test]
fn a_finished_code_child_with_uncommitted_work_can_be_abandoned() {
    let fixture = project_code_child_fixture();
    // The child finishes without committing: the reviewed revision is still the
    // base revision, so there is nothing to integrate while the checkout holds
    // uncommitted work.
    let uncommitted = fixture.child_checkout.join("uncommitted.txt");
    fs::write(&uncommitted, "uncommitted work\n").unwrap();
    complete_project_code_child(&fixture);
    assert_eq!(
        review_project_code_child(&fixture),
        fixture.worktree.base_revision
    );

    let completion_guard = project_completion_guard(&fixture);
    let blocker = completion_guard
        .completion_blocker()
        .expect("uncommitted child work must block completion");
    assert!(blocker.contains("uncommitted work"), "{blocker}");
    assert!(
        blocker.contains("abandon"),
        "the guard should name the manager's escape: {blocker}"
    );

    let tools = code_child_manager_tools(&fixture);
    let abandon = tools.execute(&ToolCall {
        id: ToolCallId::new(),
        name: "control_project_child".to_owned(),
        arguments: serde_json::json!({
            "task_id": fixture.task_id,
            "action": "abandon",
        }),
    });
    assert!(abandon.success, "{}", abandon.output);
    let abandon: serde_json::Value = serde_json::from_str(&abandon.output).unwrap();
    assert_eq!(abandon["status"], "failed");
    assert_eq!(
        abandon["uncommitted_paths"],
        serde_json::json!(["uncommitted.txt"]),
        "the outcome must name what the manager left behind"
    );
    assert!(
        abandon["note"]
            .as_str()
            .is_some_and(|note| note.contains("without landing its work")),
        "the outcome must say the work will not be landed: {}",
        abandon["note"]
    );

    // The task is closed, the guard no longer holds, and the uncommitted work is
    // still on disk for inspection.
    assert_eq!(
        fixture
            .backend
            .persistence
            .as_ref()
            .unwrap()
            .load_delegated_task(fixture.task_id)
            .unwrap()
            .unwrap()
            .status,
        loom_core::DelegatedTaskStatus::Failed
    );
    let worktree = integrated_worktree(&fixture);
    assert_eq!(worktree.status, ProjectWorktreeStatus::Ready);
    assert!(
        worktree
            .error
            .as_deref()
            .is_some_and(|error| error.contains("uncommitted.txt")
                && error.contains("will not be integrated")),
        "the abandonment should be recorded with its uncommitted paths and state that they will not be landed"
    );
    assert_eq!(
        completion_guard.completion_blocker(),
        None,
        "abandoning the child must release the completion guard"
    );
    assert_eq!(
        fs::read_to_string(&uncommitted).unwrap(),
        "uncommitted work\n"
    );

    // A closed child can no longer be integrated, and a retried abandonment
    // reports the same paths instead of changing anything.
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
    assert!(matches!(integration.result, Err(error) if error.code == ErrorCode::InvalidState));
    let repeated = tools.execute(&ToolCall {
        id: ToolCallId::new(),
        name: "control_project_child".to_owned(),
        arguments: serde_json::json!({
            "task_id": fixture.task_id,
            "action": "abandon",
        }),
    });
    assert!(repeated.success, "{}", repeated.output);
    let repeated: serde_json::Value = serde_json::from_str(&repeated.output).unwrap();
    assert_eq!(repeated["uncommitted_paths"], abandon["uncommitted_paths"]);
    fixture.shutdown();
}

#[test]
fn abandoning_a_live_child_is_refused() {
    let fixture = project_code_child_fixture();
    let abandon = fixture
        .connection
        .request(RequestEnvelope::new(ClientRequest::Project(
            ProjectRequest::ControlProjectChild {
                project_id: fixture.project_id,
                manager_session_id: fixture.root_id,
                task_id: fixture.task_id,
                action: ProjectChildControlAction::Abandon,
            },
        )));
    let Err(error) = abandon.result else {
        panic!("a queued child with no finished run cannot be abandoned");
    };
    assert_eq!(error.code, ErrorCode::InvalidState);
    assert!(error.message.contains("still live"), "{}", error.message);
    assert_eq!(
        fixture
            .backend
            .persistence
            .as_ref()
            .unwrap()
            .load_delegated_task(fixture.task_id)
            .unwrap()
            .unwrap()
            .status,
        loom_core::DelegatedTaskStatus::Queued
    );
    fixture.shutdown();
}

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
        goal: None,
        state: AgentSessionState::Idle,
        created_at,
        updated_at: created_at,
        title_source: loom_core::SessionTitleSource::Auto,
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
        base_refresh: None,
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
fn project_child_replays_its_own_commit_onto_an_advanced_parent() {
    let fixture = project_code_child_fixture();
    commit_file(
        &fixture.child_checkout,
        "child.txt",
        "child\n",
        "feat: child change\n\nwhy the child changed",
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
    // The parent tip is the child's own replayed commit: neither Loom's
    // bookkeeping commit nor a merge commit of Loom's own.
    assert_ne!(integrated_revision, parent_advanced);
    assert_ne!(integrated_revision, child_revision);
    assert_eq!(
        fixture.parent_git.status().unwrap().head.as_deref(),
        Some(integrated_revision.as_str())
    );
    assert_eq!(
        integrated.result_revision.as_deref(),
        Some(integrated_revision.as_str()),
        "the replayed revision is the reviewed revision the child is on"
    );
    assert_eq!(
        git_output(fixture.parent_git.root(), &["rev-parse", "HEAD~1"]),
        parent_advanced
    );
    assert_eq!(
        git_output(
            fixture.parent_git.root(),
            &["rev-list", "--parents", "-n1", &integrated_revision]
        ),
        format!("{integrated_revision} {parent_advanced}"),
        "the landed commit is a single-parent commit on the parent tip"
    );
    assert_eq!(
        git_output(
            fixture.parent_git.root(),
            &["log", "-1", "--format=%B", &integrated_revision]
        ),
        "feat: child change\n\nwhy the child changed",
        "the landed commit carries the child's own message"
    );
    assert_eq!(
        git_output(
            fixture.parent_git.root(),
            &["log", "-1", "--format=%an <%ae>", &integrated_revision]
        ),
        "Loom Test <loom@example.test>",
        "the landed commit keeps the child's author"
    );
    let subjects = git_output(fixture.parent_git.root(), &["log", "--format=%s"]);
    assert!(
        !subjects.contains("loom: integrate project child task"),
        "no bookkeeping integration commit may reach the parent history: {subjects}"
    );
    assert_eq!(
        fs::read_to_string(fixture.parent_git.root().join("README.md")).unwrap(),
        "parent advanced\n"
    );
    assert_eq!(
        fs::read_to_string(fixture.parent_git.root().join("child.txt")).unwrap(),
        "child\n"
    );
    // The child's branch is the replayed revision, which is what the parent
    // fast-forwarded to, so its checkout carries the parent's advance too.
    assert_eq!(
        git_output(&fixture.child_checkout, &["rev-parse", "HEAD"]),
        integrated_revision
    );
    assert_eq!(
        fs::read_to_string(fixture.child_checkout.join("README.md")).unwrap(),
        "parent advanced\n"
    );
    let child_git = GitService::open(&fixture.child_checkout).unwrap();
    assert!(child_git.status().unwrap().clean);

    // The guard's intent is that the reviewed result is contained in what was
    // integrated. A replay moves the child's branch, so the recorded reviewed
    // revision follows it and a replay integration still satisfies it.
    let completion_guard = project_completion_guard(&fixture);
    assert_eq!(completion_guard.completion_blocker(), None);
    fixture.shutdown();
}

#[test]
fn project_child_with_a_merge_in_its_branch_carries_its_own_message() {
    let fixture = project_code_child_fixture();
    // Replaying this child would flatten its own merge, so it is integrated
    // with a merge whose message is the child's own with a task trailer.
    let side_branch = "child-side";
    git_in(&fixture.child_checkout, &["branch", side_branch]);
    commit_file(
        &fixture.child_checkout,
        "child.txt",
        "child\n",
        "child change",
    );
    git_in(&fixture.child_checkout, &["checkout", "-q", side_branch]);
    commit_file(&fixture.child_checkout, "side.txt", "side\n", "side change");
    git_in(
        &fixture.child_checkout,
        &["checkout", "-q", &fixture.worktree.branch_name],
    );
    git_in(
        &fixture.child_checkout,
        &["merge", "--no-ff", "-m", "merge side work", side_branch],
    );
    complete_project_code_child(&fixture);
    let child_revision = review_project_code_child(&fixture);
    assert_eq!(
        git_output(
            &fixture.child_checkout,
            &["log", "-1", "--format=%s", &child_revision]
        ),
        "merge side work",
        "the child's own commit should be the merge it made"
    );

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
        git_output(
            fixture.parent_git.root(),
            &["rev-list", "--parents", "-n1", &integrated_revision]
        ),
        format!("{integrated_revision} {parent_advanced} {child_revision}"),
        "expected a two-parent merge commit"
    );
    assert_eq!(
        git_output(
            fixture.parent_git.root(),
            &["log", "-1", "--format=%B", &integrated_revision]
        ),
        format!("merge side work\n\nLoom-Task: {}", fixture.task_id),
        "the merge carries the child's own message and the task trailer"
    );
    assert_eq!(
        fs::read_to_string(fixture.parent_git.root().join("child.txt")).unwrap(),
        "child\n"
    );
    assert_eq!(
        fs::read_to_string(fixture.parent_git.root().join("side.txt")).unwrap(),
        "side\n"
    );
    fixture.shutdown();
}

#[test]
fn project_child_completion_guard_explains_why_a_reviewed_result_is_unintegrated() {
    let fixture = project_code_child_fixture();
    commit_file(
        &fixture.child_checkout,
        "child.txt",
        "child\n",
        "child change",
    );
    complete_project_code_child(&fixture);
    let child_revision = review_project_code_child(&fixture);

    let completion_guard = project_completion_guard(&fixture);
    let blocker = completion_guard
        .completion_blocker()
        .expect("an unintegrated reviewed result must block completion");
    assert!(
        blocker.contains("no integration is recorded"),
        "unexpected blocker: {blocker}"
    );

    commit_file(
        fixture.parent_git.root(),
        "README.md",
        "parent advanced\n",
        "parent change",
    );
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
    assert_eq!(completion_guard.completion_blocker(), None);

    // A child that advances after integration is un-integrated again; the guard
    // must name the stale integration rather than repeat a bare assertion.
    commit_file(
        &fixture.child_checkout,
        "child2.txt",
        "child follow-up\n",
        "child follow-up",
    );
    let advanced_child_revision = review_project_code_child(&fixture);
    assert_ne!(advanced_child_revision, child_revision);
    let blocker = completion_guard
        .completion_blocker()
        .expect("a child that advanced after integration must block completion");
    assert!(
        blocker.contains("does not contain the reviewed result"),
        "the blocker should name the stale integration: {blocker}"
    );

    // When the parent repository cannot be resolved the guard must report that
    // it could not confirm integration rather than asserting the result is
    // un-integrated.
    fixture.backend.session_vcs().unwrap().remove(&(
        fixture.worktree.parent_session_id,
        fixture.worktree.parent_repository_id,
    ));
    fixture
        .backend
        .session_repositories()
        .unwrap()
        .get_mut(&fixture.worktree.parent_session_id)
        .unwrap()
        .remove(&fixture.worktree.parent_repository_id);
    let blocker = completion_guard
        .completion_blocker()
        .expect("an unconfirmable integration must block completion");
    assert!(
        blocker.contains("could not be confirmed"),
        "the blocker should report the unconfirmable state: {blocker}"
    );
    fixture.shutdown();
}

#[test]
fn project_child_replay_conflict_is_refused_and_preserved() {
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
    // The refused replay leaves the child where it was: same revision, same
    // branch, a clean checkout, and no rebase in progress.
    assert_eq!(
        git_output(&fixture.child_checkout, &["rev-parse", "HEAD"]),
        child_revision
    );
    assert_eq!(
        git_output(&fixture.child_checkout, &["branch", "--show-current"]),
        fixture.worktree.branch_name
    );
    let child_git = GitService::open(&fixture.child_checkout).unwrap();
    assert!(!child_git.operation_in_progress().unwrap());
    let child_status = child_git.status().unwrap();
    assert!(child_status.clean);
    assert!(child_status.conflicts.is_empty());
    let child_git_dir = git_output(
        &fixture.child_checkout,
        &["rev-parse", "--absolute-git-dir"],
    );
    assert!(
        !std::path::Path::new(&child_git_dir)
            .join("rebase-merge")
            .exists(),
        "a refused replay must not leave the child branch mid-rebase"
    );
    assert!(
        !std::path::Path::new(&child_git_dir)
            .join("REBASE_HEAD")
            .exists(),
        "a refused replay must not leave the child branch mid-rebase"
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
fn child_creation_reports_how_far_its_base_is_behind_the_remote() {
    let mut fixture = pending_project_code_child_fixture();
    // Upstream moves after the parent checkout was cloned, which is the case
    // where the cached `origin/*` ref still looks current.
    commit_file(
        &fixture.source,
        "upstream.txt",
        "upstream change\n",
        "upstream change",
    );

    create_project_code_child_checkout(&mut fixture);

    assert_eq!(fixture.worktree.status, ProjectWorktreeStatus::Ready);
    let refresh = fixture
        .worktree
        .base_refresh
        .clone()
        .expect("child creation should record its base refresh");
    let ProjectWorktreeBaseRefresh::Fetched {
        remote,
        base_revision,
        ahead,
        behind,
        default_branch_revision,
        ..
    } = refresh
    else {
        panic!("expected a fetched base refresh, found {refresh:?}");
    };
    assert_eq!(remote, "origin");
    assert_eq!(base_revision, fixture.worktree.base_revision);
    assert_eq!((ahead, behind), (0, 1));
    assert_ne!(
        default_branch_revision, base_revision,
        "the report must name the fetched tip, not the base"
    );

    // The manager sees the drift in the delegation result, not only in storage.
    let base = project_completion_guard(&fixture)
        .child_checkout_base(fixture.task_id)
        .expect("the recorded base refresh should be reported to the manager");
    assert_eq!(base["base_revision"], serde_json::json!(base_revision));
    assert!(
        base["summary"]
            .as_str()
            .is_some_and(|summary| summary.contains("is 1 commit ahead")),
        "summary should name the drift: {base}"
    );
    fixture.shutdown();
}

#[test]
fn child_creation_proceeds_with_a_warning_when_the_refresh_fails() {
    let mut fixture = pending_project_code_child_fixture();
    // The remote the parent checkout was cloned from is no longer reachable, so
    // the bounded refresh cannot resolve the default-branch tip.
    git_in(
        fixture.parent_git.root(),
        &["remote", "set-url", "origin", "/nonexistent/loom-origin"],
    );

    create_project_code_child_checkout(&mut fixture);

    assert_eq!(fixture.worktree.status, ProjectWorktreeStatus::Ready);
    let refresh = fixture
        .worktree
        .base_refresh
        .clone()
        .expect("child creation should record its base refresh attempt");
    let ProjectWorktreeBaseRefresh::Unavailable {
        remote,
        base_revision,
        warning,
        ..
    } = refresh
    else {
        panic!("expected an unavailable base refresh, found {refresh:?}");
    };
    assert_eq!(remote, "origin");
    assert_eq!(base_revision, fixture.worktree.base_revision);
    assert!(
        warning.contains("origin"),
        "the warning should name the remote that failed: {warning}"
    );

    // The child checkout still exists on the durable branch and base revision.
    let child_git = GitService::open(&fixture.child_checkout).unwrap();
    assert_eq!(
        child_git.status().unwrap().head.as_deref(),
        Some(fixture.worktree.base_revision.as_str())
    );
    assert_eq!(
        child_git.current_branch().unwrap().as_deref(),
        Some(fixture.worktree.branch_name.as_str())
    );
    fixture.shutdown();
}

#[test]
fn delegated_child_instructions_carry_the_recorded_base_freshness() {
    let mut fixture = pending_project_code_child_fixture();
    // Upstream moves after the parent checkout was cloned, so the child's
    // recorded base is one commit behind the fetched default-branch tip.
    commit_file(
        &fixture.source,
        "upstream.txt",
        "upstream change\n",
        "upstream change",
    );
    create_project_code_child_checkout(&mut fixture);
    let recorded = fixture
        .worktree
        .base_refresh
        .clone()
        .expect("child creation should record its base refresh");

    let mut task = fixture
        .backend
        .persistence
        .as_ref()
        .unwrap()
        .load_delegated_task(fixture.task_id)
        .unwrap()
        .expect("the child task should be durable");
    fixture
        .connection
        .schedule_project_task_if_ready_under_admission(&mut task)
        .unwrap();

    let run_id = fixture
        .backend
        .persistence
        .as_ref()
        .unwrap()
        .load_latest_run_summary_for_session(fixture.worktree.child_session_id)
        .unwrap()
        .expect("the scheduled child should have started a run")
        .snapshot
        .id;
    let instructions = fixture
        .backend
        .persistence
        .as_ref()
        .unwrap()
        .load_run_runtime_config(run_id)
        .unwrap()
        .expect("the child run should be persisted with its runtime config")
        .system_instructions
        .expect("the child run should carry system instructions");

    assert!(
        instructions.contains("is 1 commit ahead at"),
        "the child should know how stale its base is: {instructions}"
    );
    assert!(
        instructions.contains(&recorded.summary()),
        "the child should carry the recorded base summary: {instructions}"
    );
    fixture.shutdown();
}

#[test]
fn delegated_code_child_is_told_its_commit_lands_through_integration() {
    let mut fixture = pending_project_code_child_fixture();
    create_project_code_child_checkout(&mut fixture);

    let mut task = fixture
        .backend
        .persistence
        .as_ref()
        .unwrap()
        .load_delegated_task(fixture.task_id)
        .unwrap()
        .expect("the child task should be durable");
    fixture
        .connection
        .schedule_project_task_if_ready_under_admission(&mut task)
        .unwrap();

    let run_id = fixture
        .backend
        .persistence
        .as_ref()
        .unwrap()
        .load_latest_run_summary_for_session(fixture.worktree.child_session_id)
        .unwrap()
        .expect("the scheduled child should have started a run")
        .snapshot
        .id;
    let instructions = fixture
        .backend
        .persistence
        .as_ref()
        .unwrap()
        .load_run_runtime_config(run_id)
        .unwrap()
        .expect("the child run should be persisted with its runtime config")
        .system_instructions
        .expect("the child run should carry system instructions");

    // The child's commit is the deliverable; publishing it would bypass the
    // reviewed integration path that lands it in the manager's checkout.
    assert!(
        instructions.contains("isolated Git worktree that lands through your parent"),
        "the child should be told how its commit lands: {instructions}"
    );
    assert!(
        instructions.contains("do not push branches or tags or open pull requests"),
        "the child should be told not to publish its branch: {instructions}"
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

#[test]
fn a_provider_failed_code_child_resumes_and_its_work_is_landed() {
    let temp = workspace();
    let source = git_repository();
    let mut model = ScriptedOpenAiEndpoint::start();
    let model_endpoint = model.endpoint.clone();
    let model_id = ModelId::new("fixture/provider-failed-child-recovery");
    let backend = InProcessBackend::with_provider_registry_persistent(
        scripted_project_provider_registry(&model_endpoint, model_id.clone()),
        temp.join("state.sqlite"),
    )
    .unwrap();
    let connection = backend.connect();
    negotiate(&connection);
    let workspace_record = match connection
        .request(RequestEnvelope::new(ClientRequest::Workspace(
            WorkspaceRequest::CreateWorkspace {
                name: "Provider-failed child recovery".to_owned(),
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
        ServerResponse::Session(SessionResponse::AgentSessionCreated(session)) => session.id,
        response => panic!("unexpected session response: {response:?}"),
    };
    let parent_repository = match connection
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
    let parent_git = connection.session_git(root, parent_repository.id).unwrap();
    let base_revision = parent_git.status().unwrap().head.unwrap();

    let (task, child) = match connection
        .create_project_child(
            RequestId::new(),
            root,
            "recovering-code-child".to_owned(),
            loom_core::DelegatedTaskSpec {
                intent: "Write the change and commit it.".to_owned(),
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
            (*task, child)
        }
        response => panic!("unexpected child creation response: {response:?}"),
    };
    let persistence = backend.persistence.as_ref().unwrap();
    let worktree = persistence
        .load_project_worktree_by_task(task.task_id)
        .unwrap()
        .expect("a code child should have a durable worktree");
    assert_eq!(worktree.status, ProjectWorktreeStatus::Ready);
    assert_eq!(worktree.base_revision, base_revision);
    let child_checkout = connection
        .session_filesystem(child.session_id)
        .unwrap()
        .root()
        .join(&worktree.relative_path);
    let child_run_id = persistence
        .load_latest_run_summary_for_session(child.session_id)
        .unwrap()
        .expect("the code child should have a durable run")
        .snapshot
        .id;

    // The child wrote its change and then failed at the provider mid-flight, so
    // its work is complete but uncommitted while its run is finished.
    fs::write(child_checkout.join("recovered.txt"), "recovered work\n").unwrap();
    fs::write(child_checkout.join("README.md"), "recovered change\n").unwrap();
    let failing_turn = model.next_for_child();
    model.respond_with_failure(failing_turn, 400, "deepseek transport failed");
    assert_eq!(
        await_settled_run(&connection, child_run_id).state,
        AgentRunState::Failed
    );
    let failed_task = persistence
        .load_delegated_task(task.task_id)
        .unwrap()
        .unwrap();
    assert_eq!(failed_task.status, loom_core::DelegatedTaskStatus::Failed);

    // The manager's own listing names the unlanded work, so the manager does not
    // have to open the worktree path to learn that it exists.
    let extension = backend
        .project_agent_tools(
            root,
            model_id.clone(),
            ProjectAgentToolGrants {
                inspection: true,
                child_control: true,
                review: true,
                integration: true,
                ..ProjectAgentToolGrants::default()
            },
        )
        .unwrap()
        .expect("manager project tools");
    let tools = ToolExecutor::new_with_workspace(connection.session_filesystem(root).unwrap())
        .with_extension(extension);
    let listing = tools.execute(&ToolCall {
        id: ToolCallId::new(),
        name: "list_project_children".to_owned(),
        arguments: serde_json::json!({}),
    });
    assert!(listing.success, "{}", listing.output);
    let listing: serde_json::Value = serde_json::from_str(&listing.output).unwrap();
    let listed = listing["children"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["task_id"] == serde_json::json!(task.task_id))
        .expect("the failed child should be listed")
        .clone();
    assert_eq!(listed["task_status"], serde_json::json!("failed"));
    assert_eq!(
        listed["worktree_uncommitted_path_count"],
        serde_json::json!(2)
    );
    assert_eq!(
        listed["worktree_uncommitted_line_delta"],
        serde_json::json!({"additions": 2, "deletions": 1}),
        "the listing should report the size of the uncommitted change: {listed}"
    );
    assert_eq!(listed["worktree_head_is_base"], serde_json::json!(true));
    assert_eq!(listed["result_revision_is_base"], serde_json::json!(true));

    // Retrying the failed run resumes it from its checkpoint through the tool
    // surface. The provider call is what failed, so there is no tool step to
    // re-run, and refusing a finished run stays the rule for `continue`.
    let retry = tools.execute(&ToolCall {
        id: ToolCallId::new(),
        name: "control_project_child".to_owned(),
        arguments: serde_json::json!({
            "task_id": task.task_id,
            "action": "retry_failed_step",
        }),
    });
    assert!(retry.success, "{}", retry.output);
    let retry: serde_json::Value = serde_json::from_str(&retry.output).unwrap();
    assert_eq!(retry["status"], serde_json::json!("running"));
    assert!(
        matches!(retry["run_state"].as_str(), Some("planning" | "executing")),
        "the provider failure should resume the run instead of refusing it: {retry}"
    );

    // The resumed child commits the work its checkout already held and finishes
    // its turn, exactly as a child that had not failed would.
    let resumed_turn = model.next_for_child();
    commit_file(
        &child_checkout,
        "recovered.txt",
        "recovered work\n",
        "fix: commit the recovered change",
    );
    git_in(&child_checkout, &["add", "--", "README.md"]);
    git_in(
        &child_checkout,
        &["commit", "-qm", "fix: recover the rewritten readme"],
    );
    model.respond_with_text(resumed_turn, "The recovered change is committed.");
    assert_eq!(
        await_settled_run(&connection, child_run_id).state,
        AgentRunState::Completed
    );
    let completed_status = {
        let mut status = failed_task.status;
        for _ in 0..1_000 {
            status = persistence
                .load_delegated_task(task.task_id)
                .unwrap()
                .unwrap()
                .status;
            if status == loom_core::DelegatedTaskStatus::Completed {
                break;
            }
            thread::sleep(Duration::from_millis(1));
        }
        status
    };
    assert_eq!(completed_status, loom_core::DelegatedTaskStatus::Completed);

    // Review reports the recovered revision, and integration lands it, so the
    // resumed child's work reaches the parent through the normal path.
    let review = tools.execute(&ToolCall {
        id: ToolCallId::new(),
        name: "review_project_child".to_owned(),
        arguments: serde_json::json!({"task_id": task.task_id}),
    });
    assert!(review.success, "{}", review.output);
    let review: serde_json::Value = serde_json::from_str(&review.output).unwrap();
    let reviewed_revision = review["data"]["worktree"]["result_revision"]
        .as_str()
        .expect("review should report the recovered revision")
        .to_owned();
    assert_ne!(reviewed_revision, base_revision);
    let integration = tools.execute(&ToolCall {
        id: ToolCallId::new(),
        name: "integrate_project_child".to_owned(),
        arguments: serde_json::json!({
            "task_id": task.task_id,
            "expected_parent_revision": base_revision,
        }),
    });
    assert!(integration.success, "{}", integration.output);
    assert_eq!(
        fs::read_to_string(parent_git.root().join("recovered.txt")).unwrap(),
        "recovered work\n"
    );
    assert_eq!(
        fs::read_to_string(parent_git.root().join("README.md")).unwrap(),
        "recovered change\n"
    );

    drop(connection);
    backend.shutdown().unwrap();
    drop(backend);
    drop(model);
    fs::remove_dir_all(temp).unwrap();
    fs::remove_dir_all(source).unwrap();
}

#[test]
fn discarding_a_dirty_child_worktree_reports_what_it_removed() {
    let fixture = project_code_child_fixture();
    commit_file(
        &fixture.child_checkout,
        "committed.txt",
        "committed\n",
        "child commit",
    );
    fs::write(fixture.child_checkout.join("README.md"), "discarded\n").unwrap();
    complete_project_code_child(&fixture);

    let cleanup = fixture
        .connection
        .request(RequestEnvelope::new(ClientRequest::Project(
            ProjectRequest::CleanupProjectChildWorktree {
                project_id: fixture.project_id,
                manager_session_id: fixture.root_id,
                task_id: fixture.task_id,
                disposition: ProjectWorktreeCleanupDisposition::DiscardChanges,
            },
        )));
    let ServerResponse::Project(ProjectResponse::ProjectChildWorktreeUpdated(removed)) =
        cleanup.result.unwrap()
    else {
        panic!("unexpected child cleanup response");
    };
    assert_eq!(removed.status, ProjectWorktreeStatus::Removed);
    let note = removed
        .error
        .expect("a discarded dirty checkout must report what it removed");
    assert!(note.contains("will not be landed"), "{note}");
    // The committed path is not discarded, so only the modified path is named.
    assert!(note.contains("1 uncommitted path(s)"), "{note}");
    assert!(note.contains("(1 added, 1 removed line(s))"), "{note}");
    assert!(!fixture.child_checkout.exists());
    fixture.shutdown();
}

#[test]
fn abandoning_a_clean_or_unreadable_checkout_reports_that_nothing_will_be_landed() {
    // A child that committed its work but whose result the manager will not
    // land: the report says so instead of only naming no paths.
    let clean = project_code_child_fixture();
    commit_file(&clean.child_checkout, "kept.txt", "kept\n", "child commit");
    complete_project_code_child(&clean);
    let abandon = code_child_manager_tools(&clean).execute(&ToolCall {
        id: ToolCallId::new(),
        name: "control_project_child".to_owned(),
        arguments: serde_json::json!({
            "task_id": clean.task_id,
            "action": "abandon",
        }),
    });
    assert!(abandon.success, "{}", abandon.output);
    let abandon: serde_json::Value = serde_json::from_str(&abandon.output).unwrap();
    assert_eq!(abandon["uncommitted_paths"], serde_json::json!([]));
    assert!(
        abandon["note"]
            .as_str()
            .is_some_and(|note| note.contains("without landing its work")),
        "a closed child must not read as landed: {}",
        abandon["note"]
    );
    let worktree = integrated_worktree(&clean);
    assert!(
        worktree
            .error
            .as_deref()
            .is_some_and(|error| error.contains("without landing its work")
                && error.contains("nothing in its checkout will be integrated")),
        "the record must state that nothing from this child is integrated: {:?}",
        worktree.error
    );
    clean.shutdown();

    // A child whose checkout is already gone is closed the same way, with the
    // missing checkout named rather than pretending it was inspected.
    let missing = project_code_child_fixture();
    complete_project_code_child(&missing);
    fs::remove_dir_all(&missing.child_checkout).unwrap();
    let abandon = code_child_manager_tools(&missing).execute(&ToolCall {
        id: ToolCallId::new(),
        name: "control_project_child".to_owned(),
        arguments: serde_json::json!({
            "task_id": missing.task_id,
            "action": "abandon",
        }),
    });
    assert!(abandon.success, "{}", abandon.output);
    let abandon: serde_json::Value = serde_json::from_str(&abandon.output).unwrap();
    assert_eq!(abandon["uncommitted_paths"], serde_json::json!([]));
    let worktree = integrated_worktree(&missing);
    assert!(
        worktree
            .error
            .as_deref()
            .is_some_and(|error| error.contains("without landing its work")
                && error.contains("could not be inspected")),
        "an unreadable checkout must be reported instead of assumed clean: {:?}",
        worktree.error
    );
    missing.shutdown();
}

#[test]
fn listing_a_code_child_reports_its_checkout_state_unless_its_run_is_progressing() {
    let fixture = project_code_child_fixture();
    let tools = code_child_manager_tools(&fixture);
    let listed_child = |tools: &ToolExecutor| {
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
            .find(|entry| entry["task_id"] == serde_json::json!(fixture.task_id))
            .expect("the child should be listed")
            .clone()
    };

    // A child that has not finished has a checkout that is still moving, so the
    // listing says nothing about its uncommitted work.
    fs::write(fixture.child_checkout.join("in-progress.txt"), "work\n").unwrap();
    let live = listed_child(&tools);
    assert_eq!(live["task_status"], serde_json::json!("queued"));
    assert_eq!(
        live["worktree_uncommitted_path_count"],
        serde_json::Value::Null
    );
    assert_eq!(live["result_revision_is_base"], serde_json::json!(true));

    // A child whose run stopped before it finished — here paused — is no longer
    // writing its checkout, so the work it left there is reported even though the
    // task is not terminal.
    let child_run_id = loom_core::RunId::new();
    fixture.persist_child_run_state(child_run_id, AgentRunState::Paused);
    let paused = listed_child(&tools);
    assert_eq!(paused["run_state"], serde_json::json!("paused"));
    assert_eq!(
        paused["worktree_uncommitted_path_count"],
        serde_json::json!(1)
    );
    assert_eq!(paused["worktree_head_is_base"], serde_json::json!(true));

    // A run that is still progressing keeps its checkout out of the listing,
    // because a count taken now would change on its next turn.
    fixture.persist_child_run_state(child_run_id, AgentRunState::Executing);
    let running = listed_child(&tools);
    assert_eq!(running["run_state"], serde_json::json!("executing"));
    assert_eq!(
        running["worktree_uncommitted_path_count"],
        serde_json::Value::Null
    );

    // Once the task is terminal, the same listing reports the uncommitted path
    // count, so the manager learns the work exists without opening the path.
    complete_project_code_child(&fixture);
    let finished = listed_child(&tools);
    assert_eq!(
        finished["worktree_uncommitted_path_count"],
        serde_json::json!(1)
    );
    assert_eq!(finished["worktree_head_is_base"], serde_json::json!(true));
    assert_eq!(finished["worktree_note"], serde_json::Value::Null);
    fixture.shutdown();

    // A checkout that cannot be inspected at all is reported as such instead of
    // reading as clean.
    let unreadable = project_code_child_fixture();
    complete_project_code_child(&unreadable);
    fs::remove_dir_all(&unreadable.child_checkout).unwrap();
    let listed = code_child_manager_tools(&unreadable).execute(&ToolCall {
        id: ToolCallId::new(),
        name: "list_project_children".to_owned(),
        arguments: serde_json::json!({}),
    });
    assert!(listed.success, "{}", listed.output);
    let listed: serde_json::Value = serde_json::from_str(&listed.output).unwrap();
    let child = listed["children"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["task_id"] == serde_json::json!(unreadable.task_id))
        .expect("the child should be listed")
        .clone();
    assert_eq!(
        child["worktree_uncommitted_path_count"],
        serde_json::Value::Null
    );
    assert!(
        child["worktree_note"]
            .as_str()
            .is_some_and(|note| note.contains("could not be inspected")),
        "an unreadable checkout must be reported: {child}"
    );
    unreadable.shutdown();

    // A removed checkout holds nothing to report.
    let removed = project_code_child_fixture();
    complete_project_code_child(&removed);
    let cleanup = removed
        .connection
        .request(RequestEnvelope::new(ClientRequest::Project(
            ProjectRequest::CleanupProjectChildWorktree {
                project_id: removed.project_id,
                manager_session_id: removed.root_id,
                task_id: removed.task_id,
                disposition: ProjectWorktreeCleanupDisposition::RemoveClean,
            },
        )));
    assert!(matches!(
        cleanup.result,
        Ok(ServerResponse::Project(
            ProjectResponse::ProjectChildWorktreeUpdated(worktree)
        )) if worktree.status == ProjectWorktreeStatus::Removed
    ));
    let listed = code_child_manager_tools(&removed).execute(&ToolCall {
        id: ToolCallId::new(),
        name: "list_project_children".to_owned(),
        arguments: serde_json::json!({}),
    });
    assert!(listed.success, "{}", listed.output);
    let listed: serde_json::Value = serde_json::from_str(&listed.output).unwrap();
    let child = listed["children"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["task_id"] == serde_json::json!(removed.task_id))
        .expect("the child should be listed")
        .clone();
    assert_eq!(child["worktree_status"], serde_json::json!("removed"));
    assert_eq!(
        child["worktree_uncommitted_path_count"],
        serde_json::Value::Null
    );
    assert_eq!(child["worktree_note"], serde_json::Value::Null);
    removed.shutdown();
}

#[test]
fn listing_a_code_child_restores_a_lazy_filesystem_without_deadlocking() {
    let fixture = project_code_child_fixture();
    fs::write(fixture.child_checkout.join("in-progress.txt"), "work\n").unwrap();
    complete_project_code_child(&fixture);
    // Persist the child filesystem record, then simulate a restored backend:
    // the record is durable but the in-memory checkout is lazy, so inspecting
    // it restores it on demand.
    fixture.backend.flush().unwrap();
    fixture
        .backend
        .persisted_session_filesystems()
        .unwrap()
        .insert(fixture.worktree.child_session_id);
    fixture
        .backend
        .session_filesystems()
        .unwrap()
        .remove(&fixture.worktree.child_session_id);
    let tools = code_child_manager_tools(&fixture);
    let listing = tools.execute(&ToolCall {
        id: ToolCallId::new(),
        name: "list_project_children".to_owned(),
        arguments: serde_json::json!({}),
    });
    assert!(listing.success, "{}", listing.output);
    let listing: serde_json::Value = serde_json::from_str(&listing.output).unwrap();
    let child = listing["children"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["task_id"] == serde_json::json!(fixture.task_id))
        .expect("the child should be listed");
    assert_eq!(
        child["worktree_uncommitted_path_count"],
        serde_json::json!(1)
    );
    assert_eq!(child["worktree_note"], serde_json::Value::Null);
    fixture.shutdown();
}

/// The manager's child-control and inspection tools for one code child fixture,
/// so a test acts through the same surface a manager uses.
fn code_child_manager_tools(fixture: &ProjectCodeChildFixture) -> ToolExecutor {
    let extension = fixture
        .backend
        .project_agent_tools(
            fixture.root_id,
            ModelId::new("deterministic/demo"),
            ProjectAgentToolGrants {
                inspection: true,
                child_control: true,
                ..ProjectAgentToolGrants::default()
            },
        )
        .unwrap()
        .expect("manager child control tools");
    ToolExecutor::new_with_workspace(
        fixture
            .connection
            .session_filesystem(fixture.root_id)
            .unwrap(),
    )
    .with_extension(extension)
}
