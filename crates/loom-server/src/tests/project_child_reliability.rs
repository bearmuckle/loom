//! In-process tests: project child reliability fixes.
//!
//! Covers recovering a stranded cancellation cascade when child creation hits
//! the pause (#246), actionable parent-checkout blockers for code delegation
//! (#203), and exposing a failed child's reason to its manager (#244).

#[allow(unused_imports)]
use super::support::*;
use super::*;

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

#[test]
fn child_creation_recovers_a_stranded_cancellation_cascade() {
    let temp = std::env::temp_dir().join(format!(
        "loom-project-cancel-recover-create-e2e-{}",
        uuid::Uuid::new_v4()
    ));
    fs::create_dir_all(&temp).unwrap();
    let persistence_path = temp.join("state.sqlite");
    let mut model = ScriptedOpenAiEndpoint::start();
    let model_endpoint = model.endpoint.clone();
    let model_id = ModelId::new("fixture/project-cancel-recover-create");
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
                name: "Project cancel recovery e2e".to_owned(),
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

    let manager_task = match connection
        .create_project_child(
            RequestId::new(),
            root,
            "manager".to_owned(),
            loom_core::DelegatedTaskSpec {
                intent: "Hold a run while the cancellation intent is stranded.".to_owned(),
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
        response => panic!("unexpected manager creation response: {response:?}"),
    };
    let manager_stream = hold_model_stream_until_cancelled(model.next_for_child());

    let project_id = ProjectId::from_uuid(*root.as_uuid());
    let persistence = backend.persistence.as_ref().unwrap();
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
        1,
        "the interrupted cancel must leave a durable intent"
    );

    // Creating another child in the same project must recover the stranded
    // cascade instead of refusing until a restart.
    let sibling_task = match connection
        .create_project_child(
            RequestId::new(),
            root,
            "sibling".to_owned(),
            loom_core::DelegatedTaskSpec {
                intent: "Confirm delegation is no longer paused.".to_owned(),
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
        response => panic!("unexpected sibling creation response: {response:?}"),
    };
    assert!(
        persistence
            .list_pending_project_cancellation_cascades()
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        persistence
            .load_delegated_task(manager_task.task_id)
            .unwrap()
            .unwrap()
            .status,
        loom_core::DelegatedTaskStatus::Cancelled,
        "recovery should cancel the manager that was the cascade root"
    );
    let _ = sibling_task;
    let sibling_stream = hold_model_stream_until_cancelled(model.next_for_child());

    drop(connection);
    backend.shutdown().unwrap();
    manager_stream.join().unwrap();
    sibling_stream.join().unwrap();
    drop(backend);
    drop(model);
    fs::remove_dir_all(temp).unwrap();
}

fn attach_single_repository(
    connection: &InProcessConnection,
    workspace_id: WorkspaceId,
    source: &Path,
    name: &str,
) -> (AgentSessionId, GitService) {
    let root = match connection
        .request(RequestEnvelope::new(ClientRequest::Workspace(
            WorkspaceRequest::CreateAgentSessionInWorkspace {
                workspace_id,
                name: "Manager".to_owned(),
            },
        )))
        .result
        .unwrap()
    {
        ServerResponse::Session(SessionResponse::AgentSessionCreated(session)) => session.id,
        response => panic!("unexpected session response: {response:?}"),
    };
    let repository = match connection
        .request(RequestEnvelope::new(ClientRequest::Repository(
            RepositoryRequest::AttachSessionRepository {
                session_id: root,
                source: source.display().to_string(),
                path: name.to_owned(),
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
    let parent_git = connection.session_git(root, repository.id).unwrap();
    (root, parent_git)
}

fn code_delegation_spec() -> loom_core::DelegatedTaskSpec {
    loom_core::DelegatedTaskSpec {
        intent: "Change code".to_owned(),
        model_id: "deterministic/demo".to_owned(),
        context_references: vec![],
        dependencies: vec![],
        code_change: true,
        permissions: loom_core::ProjectAgentPermissions::default(),
    }
}

#[test]
fn code_delegation_reports_a_dirty_parent_checkout_and_its_paths() {
    let temp = workspace();
    let source = git_repository();
    let database_path = temp.join("state.sqlite");
    let backend = InProcessBackend::new_persistent(&database_path).unwrap();
    let connection = backend.connect();
    negotiate(&connection);
    let workspace_record = match connection
        .request(RequestEnvelope::new(ClientRequest::Workspace(
            WorkspaceRequest::CreateWorkspace {
                name: "Dirty parent".to_owned(),
            },
        )))
        .result
        .unwrap()
    {
        ServerResponse::Workspace(WorkspaceResponse::WorkspaceCreated(workspace)) => workspace,
        response => panic!("unexpected workspace response: {response:?}"),
    };
    let (root, parent_git) =
        attach_single_repository(&connection, workspace_record.id, &source, "repo");
    fs::write(parent_git.root().join("uncommitted-build.log"), "dirty\n").unwrap();

    let error = connection
        .create_project_child(
            RequestId::new(),
            root,
            "code-child".to_owned(),
            code_delegation_spec(),
        )
        .unwrap_err();
    assert_eq!(error.code, ErrorCode::Conflict);
    assert!(
        error.message.contains("uncommitted-build.log"),
        "the blocker should be named: {}",
        error.message
    );
    assert!(
        error.message.contains("commit, stash"),
        "the message should say how to recover: {}",
        error.message
    );

    drop(connection);
    backend.shutdown().unwrap();
    fs::remove_dir_all(temp).unwrap();
    fs::remove_dir_all(source).unwrap();
}

#[test]
fn code_delegation_reports_a_parent_checkout_without_commits() {
    let temp = workspace();
    let source = workspace();
    git_in(&source, &["init", "-q"]);
    git_in(&source, &["config", "user.name", "Loom Test"]);
    git_in(&source, &["config", "user.email", "loom@example.test"]);
    let database_path = temp.join("state.sqlite");
    let backend = InProcessBackend::new_persistent(&database_path).unwrap();
    let connection = backend.connect();
    negotiate(&connection);
    let workspace_record = match connection
        .request(RequestEnvelope::new(ClientRequest::Workspace(
            WorkspaceRequest::CreateWorkspace {
                name: "Empty parent".to_owned(),
            },
        )))
        .result
        .unwrap()
    {
        ServerResponse::Workspace(WorkspaceResponse::WorkspaceCreated(workspace)) => workspace,
        response => panic!("unexpected workspace response: {response:?}"),
    };
    let (root, _parent_git) =
        attach_single_repository(&connection, workspace_record.id, &source, "repo");

    let error = connection
        .create_project_child(
            RequestId::new(),
            root,
            "code-child".to_owned(),
            code_delegation_spec(),
        )
        .unwrap_err();
    assert_eq!(error.code, ErrorCode::Conflict);
    assert!(
        error.message.contains("no commit"),
        "the message should name the missing commit: {}",
        error.message
    );

    drop(connection);
    backend.shutdown().unwrap();
    fs::remove_dir_all(temp).unwrap();
    fs::remove_dir_all(source).unwrap();
}

#[test]
fn integration_ignores_ignored_build_output_in_the_parent_checkout() {
    let fixture = project_code_child_fixture();
    let parent_git = &fixture.parent_git;
    // Commit a .gitignore so a build directory in the parent is ignored.
    fs::write(parent_git.root().join(".gitignore"), "target/\n").unwrap();
    git_in(parent_git.root(), &["add", "--", ".gitignore"]);
    git_in(parent_git.root(), &["commit", "-qm", "ignore build output"]);
    // The child commits a real change on its own branch.
    git_in(
        &fixture.child_checkout,
        &["config", "user.name", "Loom Test"],
    );
    git_in(
        &fixture.child_checkout,
        &["config", "user.email", "loom@example.test"],
    );
    fs::write(fixture.child_checkout.join("README.md"), "child result\n").unwrap();
    git_in(&fixture.child_checkout, &["add", "--", "README.md"]);
    git_in(&fixture.child_checkout, &["commit", "-qm", "child result"]);
    complete_project_code_child(&fixture);
    review_project_code_child(&fixture);
    // A gitignored build directory must not block integration.
    fs::create_dir_all(parent_git.root().join("target")).unwrap();
    fs::write(parent_git.root().join("target/artifact"), "build output\n").unwrap();

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
    assert_eq!(
        fs::read_to_string(parent_git.root().join("target/artifact")).unwrap(),
        "build output\n"
    );
    fixture.shutdown();
}

#[test]
fn integration_names_blocking_parent_paths() {
    let fixture = project_code_child_fixture();
    let parent_git = &fixture.parent_git;
    git_in(
        &fixture.child_checkout,
        &["config", "user.name", "Loom Test"],
    );
    git_in(
        &fixture.child_checkout,
        &["config", "user.email", "loom@example.test"],
    );
    fs::write(fixture.child_checkout.join("README.md"), "child result\n").unwrap();
    git_in(&fixture.child_checkout, &["add", "--", "README.md"]);
    git_in(&fixture.child_checkout, &["commit", "-qm", "child result"]);
    complete_project_code_child(&fixture);
    review_project_code_child(&fixture);
    // A tracked local edit must be refused, and the path must be named.
    fs::write(parent_git.root().join("README.md"), "local edit\n").unwrap();

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
    let error = integration.result.unwrap_err();
    assert_eq!(error.code, ErrorCode::Conflict);
    assert!(
        error.message.contains("README.md"),
        "the blocking path should be named: {}",
        error.message
    );
    fixture.shutdown();
}

#[test]
fn failed_child_reports_its_reason_to_the_manager() {
    let temp = std::env::temp_dir().join(format!(
        "loom-project-child-failure-e2e-{}",
        uuid::Uuid::new_v4()
    ));
    fs::create_dir_all(&temp).unwrap();
    let persistence_path = temp.join("state.sqlite");
    let mut model = ScriptedOpenAiEndpoint::start();
    let model_endpoint = model.endpoint.clone();
    let model_id = ModelId::new("fixture/project-child-failure");
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
                name: "Failed child reason e2e".to_owned(),
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

    let child_task = match connection
        .create_project_child(
            RequestId::new(),
            root,
            "failing-child".to_owned(),
            loom_core::DelegatedTaskSpec {
                intent: "Fail fast so the manager can see why.".to_owned(),
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
        response => panic!("unexpected child creation response: {response:?}"),
    };
    let persistence = backend.persistence.as_ref().unwrap();
    let run_id = persistence
        .load_latest_run_summary_for_session(child_task.target_session_id)
        .unwrap()
        .expect("child run should be durable")
        .snapshot
        .id;
    let failed_turn = model.next_for_child();
    model.respond_with_failure(failed_turn, 400, "provider exploded");
    assert_eq!(
        await_settled_run(&connection, run_id).state,
        AgentRunState::Failed
    );

    let tools = ToolExecutor::new_with_workspace(connection.session_filesystem(root).unwrap())
        .with_extension(
            backend
                .project_agent_tools(
                    root,
                    model_id.clone(),
                    ProjectAgentToolGrants {
                        delegation: false,
                        messaging: false,
                        branch_messaging: false,
                        inspection: true,
                        child_control: false,
                        worktree: false,
                        review: false,
                        integration: false,
                    },
                )
                .unwrap()
                .expect("inspection tools"),
        );
    let inspection = tools.execute(&ToolCall {
        id: ToolCallId::new(),
        name: "list_project_children".to_owned(),
        arguments: serde_json::json!({}),
    });
    assert!(inspection.success, "{}", inspection.output);
    let inspection: serde_json::Value = serde_json::from_str(&inspection.output).unwrap();
    let task_id = child_task.task_id.to_string();
    let child = inspection["children"]
        .as_array()
        .unwrap()
        .iter()
        .find(|child| child["task_id"].as_str() == Some(task_id.as_str()))
        .expect("failed child should be listed");
    assert_eq!(child["task_status"].as_str(), Some("failed"));
    assert_eq!(child["run_state"].as_str(), Some("failed"));
    let reason = child["run_summary"].as_str().unwrap_or_default();
    assert!(
        reason.contains("provider exploded"),
        "the failure reason should be exposed: {reason}"
    );

    drop(connection);
    backend.shutdown().unwrap();
    drop(backend);
    drop(model);
    fs::remove_dir_all(temp).unwrap();
}
