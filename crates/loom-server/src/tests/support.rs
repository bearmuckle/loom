//! Shared setup and scripted fixtures for the embedded in-process tests.

use super::*;

pub(super) fn request_id_with_issued_at(issued_at_ms: u64) -> loom_core::RequestId {
    let mut bytes = *loom_core::RequestId::new().as_uuid().as_bytes();
    bytes[..6].copy_from_slice(&issued_at_ms.to_be_bytes()[2..]);
    loom_core::RequestId::from_uuid(uuid::Uuid::from_bytes(bytes))
}

pub(super) fn negotiate(connection: &InProcessConnection) {
    let response = connection.request(RequestEnvelope::new(ClientRequest::Control(
        ControlRequest::Negotiate {
            client_version: CURRENT_PROTOCOL_VERSION,
            capabilities: connection.backend.supported_capabilities.clone(),
        },
    )));
    assert!(matches!(
        response.result,
        Ok(ServerResponse::Control(ControlResponse::Negotiated(_)))
    ));
}

pub(super) fn negotiate_m2(connection: &InProcessConnection) {
    negotiate(connection);
}

pub(super) fn negotiate_m3(connection: &InProcessConnection) {
    negotiate(connection);
}

pub(super) fn negotiate_m5(connection: &InProcessConnection) {
    negotiate(connection);
}

pub(super) fn respond_http(mut stream: TcpStream, status: &str, body: &str) {
    let mut request = Vec::new();
    let mut byte = [0_u8; 1];
    while !request.ends_with(b"\r\n\r\n") {
        stream.read_exact(&mut byte).unwrap();
        request.push(byte[0]);
    }
    let request = String::from_utf8_lossy(&request);
    let headers = request.to_ascii_lowercase();
    assert!(headers.contains("authorization: bearer fixture-token"));
    assert!(headers.contains("x-github-api-version: 2022-11-28"));
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(response.as_bytes()).unwrap();
}

pub(super) fn github_repository_json(name: &str) -> serde_json::Value {
    serde_json::json!({
        "full_name": name,
        "description": null,
        "clone_url": format!("https://github.com/{name}.git"),
        "private": false,
        "default_branch": "main"
    })
}

/// Waits until a run stops needing the model, because a run is now driven by
/// its own worker rather than by the request that started it.
pub(super) fn await_settled_run(
    connection: &InProcessConnection,
    run_id: loom_core::RunId,
) -> loom_agent::AgentRunSnapshot {
    // Wait on the run handle's idle condition instead of polling: the worker
    // signals `idle` when it stops running, so this wakes as soon as the run
    // settles rather than after an arbitrary sleep.
    if let Some(handle) = connection
        .backend
        .runs()
        .ok()
        .and_then(|runs| runs.get(&run_id).cloned())
    {
        let _ = handle.wait_until_idle_for(Duration::from_secs(10));
    }
    let mut last_snapshot = None;
    for _ in 0..1_000 {
        let response = connection.request(RequestEnvelope::new(ClientRequest::Run(
            RunRequest::GetAgentRun { run_id },
        )));
        let snapshot = match response.result {
            Ok(ServerResponse::Run(RunResponse::AgentRun(snapshot))) => snapshot,
            result => panic!("unexpected run response for {run_id}: {result:?}"),
        };
        last_snapshot = Some(snapshot.clone());
        if !matches!(
            snapshot.state,
            AgentRunState::Planning | AgentRunState::Executing | AgentRunState::Evaluating
        ) {
            return snapshot;
        }
        thread::sleep(Duration::from_millis(1));
    }
    let failure = connection
        .backend
        .runs()
        .ok()
        .and_then(|runs| runs.get(&run_id).cloned())
        .and_then(|handle| handle.failure());
    panic!("agent run did not settle: {last_snapshot:?}; failure: {failure:?}");
}

pub(super) fn await_project_manager_wait_status(
    persistence: &dyn Persistence,
    wait_id: loom_core::ProjectManagerWaitId,
    expected_status: loom_core::ProjectManagerWaitStatus,
) -> loom_core::ProjectManagerWaitRecord {
    let mut last_status = None;
    for _ in 0..400 {
        let wait = persistence
            .load_project_manager_wait(wait_id)
            .unwrap()
            .expect("project manager wait should remain durable");
        last_status = Some(wait.status);
        if wait.status == expected_status {
            return wait;
        }
        thread::sleep(Duration::from_millis(1));
    }
    panic!(
        "project manager wait {wait_id} did not reach {expected_status:?}; last status: {last_status:?}"
    );
}

pub(super) fn workspace() -> PathBuf {
    let root = std::env::temp_dir().join(format!("loom-server-{}", AgentSessionId::new()));
    fs::create_dir(&root).unwrap();
    root
}

pub(super) fn git_repository() -> PathBuf {
    let root = workspace();
    let run = |arguments: &[&str]| {
        assert!(
            Command::new("git")
                .args(["-C", root.to_str().unwrap()])
                .args(arguments)
                .status()
                .unwrap()
                .success()
        );
    };
    run(&["init", "-q"]);
    run(&["config", "user.name", "Loom Test"]);
    run(&["config", "user.email", "loom@example.test"]);
    fs::write(root.join("README.md"), "source\n").unwrap();
    run(&["add", "--", "README.md"]);
    run(&["commit", "-qm", "initial"]);
    root
}

pub(super) struct ProjectCodeChildFixture {
    pub(super) temp: PathBuf,
    pub(super) source: PathBuf,
    pub(super) backend: Arc<InProcessBackend>,
    pub(super) connection: InProcessConnection,
    pub(super) root_id: AgentSessionId,
    pub(super) project_id: ProjectId,
    pub(super) task_id: loom_core::TaskId,
    pub(super) worktree: ProjectWorktreeRecord,
    pub(super) child_checkout: PathBuf,
    pub(super) parent_git: GitService,
}

impl ProjectCodeChildFixture {
    pub(super) fn shutdown(self) {
        self.backend.shutdown().unwrap();
        fs::remove_dir_all(&self.temp).unwrap();
        fs::remove_dir_all(&self.source).unwrap();
    }
}

pub(super) fn git_in(dir: &Path, arguments: &[&str]) {
    let output = Command::new("git")
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .env_remove("GIT_COMMON_DIR")
        .args(["-C", dir.to_str().unwrap()])
        .args(arguments)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {:?} failed: {}",
        arguments,
        String::from_utf8_lossy(&output.stderr)
    );
}

pub(super) fn commit_file(dir: &Path, file: &str, content: &str, message: &str) {
    fs::write(dir.join(file), content).unwrap();
    git_in(dir, &["config", "user.name", "Loom Test"]);
    git_in(dir, &["config", "user.email", "loom@example.test"]);
    git_in(dir, &["add", "--", file]);
    git_in(dir, &["commit", "-qm", message]);
}

pub(super) fn project_code_child_fixture() -> ProjectCodeChildFixture {
    let temp = workspace();
    let source = git_repository();
    let database_path = temp.join("state.sqlite");
    let backend = InProcessBackend::new_persistent(&database_path).unwrap();
    let connection = backend.connect();
    negotiate(&connection);
    let workspace_record = match connection
        .request(RequestEnvelope::new(ClientRequest::Workspace(
            WorkspaceRequest::CreateWorkspace {
                name: "Merge integration".to_owned(),
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

    ProjectCodeChildFixture {
        temp,
        source,
        backend,
        connection,
        root_id: root.id,
        project_id: task.project_id,
        task_id,
        worktree,
        child_checkout,
        parent_git,
    }
}

pub(super) fn complete_project_code_child(fixture: &ProjectCodeChildFixture) {
    fixture
        .backend
        .persistence
        .as_ref()
        .unwrap()
        .update_delegated_task_status(
            fixture.task_id,
            loom_core::DelegatedTaskStatus::Completed,
            Timestamp::now(),
        )
        .unwrap();
}

pub(super) fn review_project_code_child(fixture: &ProjectCodeChildFixture) -> String {
    let response = fixture
        .connection
        .request(RequestEnvelope::new(ClientRequest::Project(
            ProjectRequest::GetProjectChildReview {
                project_id: fixture.project_id,
                manager_session_id: fixture.root_id,
                task_id: fixture.task_id,
            },
        )));
    let ServerResponse::Project(ProjectResponse::ProjectChildReview { status, .. }) =
        response.result.unwrap()
    else {
        panic!("unexpected child review response");
    };
    status.head.expect("reviewed child should have a HEAD")
}

pub(super) fn integrated_worktree(fixture: &ProjectCodeChildFixture) -> ProjectWorktreeRecord {
    fixture
        .backend
        .persistence
        .as_ref()
        .unwrap()
        .load_project_worktree_by_task(fixture.task_id)
        .unwrap()
        .expect("child worktree should be durable")
}

pub(super) fn dependency_failure_blocks_child_and_releases_manager_wait_once(
    prerequisite_fails: bool,
) {
    let temp = std::env::temp_dir().join(format!(
        "loom-project-manager-wait-cancelled-dependency-e2e-{}",
        uuid::Uuid::new_v4()
    ));
    fs::create_dir_all(&temp).unwrap();
    let persistence_path = temp.join("state.sqlite");
    let mut model = ScriptedOpenAiEndpoint::start();
    let model_endpoint = model.endpoint.clone();
    let model_id = ModelId::new("fixture/project-manager-wait-cancelled-dependency");
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
                name: "Failed dependency manager wait e2e".to_owned(),
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
                intent: "Delegate one task, wait for its result, and report it.".to_owned(),
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

    // The older direct child becomes the prerequisite and takes the only
    // slot when the manager parks. Cancelling it makes the dependent task
    // Blocked, which must release the manager's wait as return-ready.
    let prerequisite = connection
        .create_project_child(
            RequestId::new(),
            root,
            "prerequisite".to_owned(),
            loom_core::DelegatedTaskSpec {
                intent: "Complete the prerequisite investigation.".to_owned(),
                model_id: model_id.as_str().to_owned(),
                context_references: vec![],
                dependencies: vec![],
                code_change: false,
                permissions: loom_core::ProjectAgentPermissions::default(),
            },
        )
        .unwrap();
    let prerequisite_task = match prerequisite {
        ServerResponse::Project(ProjectResponse::ProjectChildCreated { task, .. }) => task,
        response => panic!("unexpected prerequisite task response: {response:?}"),
    };
    assert_eq!(
        prerequisite_task.status,
        loom_core::DelegatedTaskStatus::Queued
    );
    let timestamp_deadline = Instant::now() + Duration::from_secs(2);
    while Timestamp::now() <= prerequisite_task.created_at {
        assert!(
            Instant::now() < timestamp_deadline,
            "clock should advance before creating the dependent child"
        );
        thread::sleep(Duration::from_millis(1));
    }

    model.respond_with_tool(
        manager_delegate,
        "delegate_project_task",
        serde_json::json!({
            "child_name": "dependent",
            "intent": "Run only after the prerequisite task completes.",
            "model_id": model_id,
            "dependencies": [prerequisite_task.task_id]
        }),
    );
    let manager_wait = model.next_for_child();
    assert!(request_has_tool(
        &manager_wait.request,
        "wait_for_project_children"
    ));

    let project_id = ProjectId::from_uuid(*root.as_uuid());
    let persistence = backend.persistence.as_ref().unwrap();
    let dependent_task = persistence
        .list_project_tasks(project_id)
        .unwrap()
        .into_iter()
        .find(|task| task.requester_session_id == manager_session.session_id)
        .expect("manager delegation should create its dependent child");
    assert!(prerequisite_task.created_at < dependent_task.created_at);
    assert_eq!(
        dependent_task.status,
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
        serde_json::json!({ "task_ids": [dependent_task.task_id] }),
    );
    let settle_deadline = Instant::now() + Duration::from_secs(5);
    let manager_after_wait = loop {
        let response = connection.request(RequestEnvelope::new(ClientRequest::Run(
            RunRequest::GetAgentRun {
                run_id: manager_run_id,
            },
        )));
        let ServerResponse::Run(RunResponse::AgentRun(snapshot)) = response.result.unwrap() else {
            panic!("unexpected manager run response");
        };
        if !matches!(
            snapshot.state,
            AgentRunState::Planning | AgentRunState::Executing | AgentRunState::Evaluating
        ) {
            break snapshot;
        }
        assert!(
            Instant::now() < settle_deadline,
            "manager did not park; wait={:?}, execution={:?}, messages={:?}",
            persistence
                .list_project_manager_waits_by_child(dependent_task.task_id)
                .unwrap(),
            persistence
                .load_run_execution_state(manager_run_id)
                .unwrap(),
            persistence
                .load_run_messages(manager_run_id)
                .unwrap()
                .into_iter()
                .map(|message| (
                    message.role,
                    message.name,
                    message.tool_call_id,
                    message.content
                ))
                .collect::<Vec<_>>()
        );
        thread::sleep(Duration::from_millis(1));
    };
    assert_eq!(manager_after_wait.state, AgentRunState::Paused);

    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let manager_status = persistence
            .load_delegated_task(manager_task.task_id)
            .unwrap()
            .unwrap()
            .status;
        let prerequisite_status = persistence
            .load_delegated_task(prerequisite_task.task_id)
            .unwrap()
            .unwrap()
            .status;
        let dependent_status = persistence
            .load_delegated_task(dependent_task.task_id)
            .unwrap()
            .unwrap()
            .status;
        if manager_status == loom_core::DelegatedTaskStatus::Blocked
            && prerequisite_status == loom_core::DelegatedTaskStatus::Running
            && dependent_status == loom_core::DelegatedTaskStatus::Queued
        {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "manager should park with only its prerequisite admitted; manager={manager_status:?}, prerequisite={prerequisite_status:?}, dependent={dependent_status:?}"
        );
        thread::sleep(Duration::from_millis(1));
    }
    let wait = persistence
        .list_project_manager_waits_by_child(dependent_task.task_id)
        .unwrap()
        .into_iter()
        .find(|wait| wait.manager_session_id == manager_session.session_id)
        .expect("manager wait should be durable");
    assert_eq!(wait.status, loom_core::ProjectManagerWaitStatus::Waiting);

    let prerequisite_turn = model.next_for_child();
    assert!(!request_has_tool(
        &prerequisite_turn.request,
        "wait_for_project_children"
    ));
    let prerequisite_stream = if prerequisite_fails {
        model.respond_with_failure(prerequisite_turn, 500, "scripted prerequisite failure");
        None
    } else {
        let stream = hold_scripted_model_stream_until_cancelled(prerequisite_turn);
        let cancel = connection.request(RequestEnvelope::new(ClientRequest::Project(
            ProjectRequest::ControlProjectChild {
                project_id,
                manager_session_id: root,
                task_id: prerequisite_task.task_id,
                action: loom_protocol::ProjectChildControlAction::Cancel,
            },
        )));
        let ServerResponse::Project(ProjectResponse::ProjectChildControlled {
            task: cancelled_prerequisite,
            run: cancelled_run,
        }) = cancel.result.unwrap()
        else {
            panic!("unexpected prerequisite cancellation response");
        };
        assert_eq!(
            cancelled_prerequisite.status,
            loom_core::DelegatedTaskStatus::Cancelled
        );
        assert!(matches!(cancelled_run, Some(run)
            if run.state == AgentRunState::Cancelled));
        Some(stream)
    };

    let prerequisite_status = if prerequisite_fails {
        loom_core::DelegatedTaskStatus::Failed
    } else {
        loom_core::DelegatedTaskStatus::Cancelled
    };
    let prerequisite_deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let status = persistence
            .load_delegated_task(prerequisite_task.task_id)
            .unwrap()
            .unwrap()
            .status;
        if status == prerequisite_status {
            break;
        }
        assert!(
            Instant::now() < prerequisite_deadline,
            "prerequisite should become {prerequisite_status:?}, got {status:?}"
        );
        thread::sleep(Duration::from_millis(1));
    }
    if let Some(stream) = prerequisite_stream {
        stream.join().unwrap();
    }

    let resumed_manager_turn = model.next_for_child();
    let wait_call_id = wait.tool_call_id.to_string();
    let wait_result_messages = resumed_manager_turn.request["messages"]
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
        .collect::<Vec<_>>();
    assert_eq!(wait_result_messages.len(), 1);
    let wait_result: serde_json::Value =
        serde_json::from_str(wait_result_messages[0]["content"].as_str().unwrap()).unwrap();
    assert_eq!(wait_result["return_ready"], true);
    assert_eq!(
        wait_result["children"][0]["task_id"],
        dependent_task.task_id.to_string()
    );
    assert_eq!(wait_result["children"][0]["status"], "blocked");
    assert_eq!(
        persistence
            .load_delegated_task(dependent_task.task_id)
            .unwrap()
            .unwrap()
            .status,
        loom_core::DelegatedTaskStatus::Blocked
    );
    assert_eq!(
        persistence
            .load_project_manager_wait(wait.wait_id)
            .unwrap()
            .unwrap()
            .status,
        loom_core::ProjectManagerWaitStatus::Resuming
    );

    // The blocked child is return-ready, but remains nonterminal for the
    // manager's completion guard. Hold its next provider turn open and
    // cancel the manager for cleanup after verifying the one replay.
    let manager_turn_stream = hold_scripted_model_stream_until_cancelled(resumed_manager_turn);
    let cancel_manager = connection.request(RequestEnvelope::new(ClientRequest::Project(
        ProjectRequest::ControlProjectChild {
            project_id,
            manager_session_id: root,
            task_id: manager_task.task_id,
            action: loom_protocol::ProjectChildControlAction::Cancel,
        },
    )));
    let ServerResponse::Project(ProjectResponse::ProjectChildControlled {
        task: cancelled_manager,
        run: cancelled_manager_run,
    }) = cancel_manager.result.unwrap()
    else {
        panic!("unexpected manager cancellation response");
    };
    assert_eq!(
        cancelled_manager.status,
        loom_core::DelegatedTaskStatus::Cancelled
    );
    assert!(matches!(cancelled_manager_run, Some(run)
        if run.id == manager_run_id && run.state == AgentRunState::Cancelled));
    manager_turn_stream.join().unwrap();
    assert_eq!(
        persistence
            .load_project_manager_wait(wait.wait_id)
            .unwrap()
            .unwrap()
            .status,
        loom_core::ProjectManagerWaitStatus::Abandoned
    );
    let durable_wait_results = persistence
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
    assert!(
        persistence
            .load_latest_run_summary_for_session(dependent_task.target_session_id)
            .unwrap()
            .is_none()
    );

    drop(connection);
    backend.shutdown().unwrap();
    drop(backend);
    drop(model);
    fs::remove_dir_all(temp).unwrap();
}

/// Serves an event stream that keeps a completion open until the client
/// gives up, so a run can be controlled while the model is still working.
pub(super) fn slow_model_endpoint() -> (String, std::sync::mpsc::Receiver<()>) {
    use std::net::TcpListener;

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let (sender, receiver) = std::sync::mpsc::channel();
    thread::spawn(move || {
        use std::io::{Read, Write};

        let Ok((mut stream, _)) = listener.accept() else {
            return;
        };
        let mut request = [0_u8; 8192];
        let _ = stream.read(&mut request);
        let _ = stream.write_all(
            b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n",
        );
        let _ =
            stream.write_all(b"data: {\"choices\":[{\"delta\":{\"content\":\"thinking\"}}]}\n\n");
        let _ = stream.flush();
        let _ = sender.send(());
        // Keep the completion open; the run must be stoppable anyway.
        for _ in 0..600 {
            if stream
                .write_all(b"data: {\"choices\":[{\"delta\":{\"content\":\".\"}}]}\n\n")
                .is_err()
            {
                return;
            }
            let _ = stream.flush();
            thread::sleep(Duration::from_millis(20));
        }
    });
    (format!("http://{address}/v1/chat/completions"), receiver)
}

pub(super) fn gated_model_endpoint(
    first_content: &str,
) -> (
    String,
    std::sync::mpsc::Receiver<()>,
    std::sync::mpsc::Sender<()>,
    std::sync::mpsc::Receiver<()>,
    std::sync::mpsc::Sender<()>,
) {
    use std::net::TcpListener;

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let (first_sender, first_receiver) = std::sync::mpsc::channel();
    let (second_sender, second_receiver) = std::sync::mpsc::channel();
    let (second_gate_sender, second_gate_receiver) = std::sync::mpsc::channel();
    let (finish_gate_sender, finish_gate_receiver) = std::sync::mpsc::channel();
    let first_event = format!(
        "data: {{\"choices\":[{{\"delta\":{{\"content\":{}}}}}]}}\n\n",
        serde_json::to_string(first_content).unwrap()
    );
    thread::spawn(move || {
        use std::io::{Read, Write};

        let Ok((mut stream, _)) = listener.accept() else {
            return;
        };
        let mut request = [0_u8; 8192];
        let _ = stream.read(&mut request);
        if stream
            .write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n",
            )
            .is_err()
            || stream.write_all(first_event.as_bytes()).is_err()
            || stream.flush().is_err()
        {
            return;
        }
        let _ = first_sender.send(());
        if second_gate_receiver.recv().is_err() {
            return;
        }
        if stream
            .write_all(b"data: {\"choices\":[{\"delta\":{\"content\":\"second\"}}]}\n\n")
            .is_err()
            || stream.flush().is_err()
        {
            return;
        }
        let _ = second_sender.send(());
        if finish_gate_receiver.recv().is_err() {
            return;
        }
        let _ = stream.write_all(b"data: [DONE]\n\n");
        let _ = stream.flush();
    });
    (
        format!("http://{address}/v1/chat/completions"),
        first_receiver,
        second_gate_sender,
        second_receiver,
        finish_gate_sender,
    )
}

pub(super) struct ScriptedModelRequest {
    pub(super) request: serde_json::Value,
    pub(super) stream: TcpStream,
}

pub(super) fn hold_scripted_model_stream_until_cancelled(
    request: ScriptedModelRequest,
) -> thread::JoinHandle<()> {
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

pub(super) struct ScriptedOpenAiEndpoint {
    pub(super) endpoint: String,
    pub(super) address: std::net::SocketAddr,
    pub(super) requests: std::sync::mpsc::Receiver<ScriptedModelRequest>,
    pub(super) pending: VecDeque<ScriptedModelRequest>,
    pub(super) stopped: Arc<AtomicBool>,
    pub(super) accept_worker: Option<thread::JoinHandle<()>>,
}

impl ScriptedOpenAiEndpoint {
    pub(super) fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        listener.set_nonblocking(true).unwrap();
        let (sender, requests) = std::sync::mpsc::channel();
        let stopped = Arc::new(AtomicBool::new(false));
        let worker_stopped = Arc::clone(&stopped);
        let accept_worker = thread::spawn(move || {
            while !worker_stopped.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        let sender = sender.clone();
                        thread::spawn(move || {
                            let Ok(body) = read_scripted_http_request_body(&mut stream) else {
                                return;
                            };
                            let Ok(request) = serde_json::from_slice(&body) else {
                                return;
                            };
                            let _ = sender.send(ScriptedModelRequest { request, stream });
                        });
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(1));
                    }
                    Err(_) => break,
                }
            }
        });
        Self {
            endpoint: format!("http://{address}/v1/chat/completions"),
            address,
            requests,
            pending: VecDeque::new(),
            stopped,
            accept_worker: Some(accept_worker),
        }
    }

    pub(super) fn next_for_manager(&mut self) -> ScriptedModelRequest {
        self.next_for_role(true)
    }

    pub(super) fn next_for_child(&mut self) -> ScriptedModelRequest {
        self.next_for_role(false)
    }

    pub(super) fn next_for_role(&mut self, manager: bool) -> ScriptedModelRequest {
        if let Some(index) = self
            .pending
            .iter()
            .position(|request| scripted_request_is_manager(&request.request) == manager)
        {
            return self.pending.remove(index).unwrap();
        }
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            assert!(
                !remaining.is_zero(),
                "timed out waiting for scripted model request"
            );
            let request = self
                .requests
                .recv_timeout(remaining)
                .expect("scripted model request did not arrive");
            if scripted_request_is_manager(&request.request) == manager {
                return request;
            }
            self.pending.push_back(request);
        }
    }

    pub(super) fn respond_with_tool(
        &self,
        request: ScriptedModelRequest,
        name: &str,
        arguments: serde_json::Value,
    ) {
        let response = serde_json::json!({
            "choices": [{
                "message": {
                    "role": "assistant",
                    "content": "",
                    "tool_calls": [{
                        "id": "fixture-call",
                        "type": "function",
                        "function": {
                            "name": name,
                            "arguments": serde_json::to_string(&arguments).unwrap()
                        }
                    }]
                },
                "finish_reason": "tool_calls"
            }]
        });
        write_scripted_http_response(request.stream, response);
    }

    pub(super) fn respond_with_text(&self, request: ScriptedModelRequest, content: &str) {
        let response = serde_json::json!({
            "choices": [{
                "message": {"role": "assistant", "content": content},
                "finish_reason": "stop"
            }]
        });
        write_scripted_http_response(request.stream, response);
    }

    pub(super) fn respond_with_failure(
        &self,
        request: ScriptedModelRequest,
        status: u16,
        message: &str,
    ) {
        let body = serde_json::json!({"error": {"message": message}}).to_string();
        let reason = match status {
            500 => "Internal Server Error",
            503 => "Service Unavailable",
            _ => "Scripted Failure",
        };
        let response = format!(
            "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        let mut stream = request.stream;
        let _ = stream.write_all(response.as_bytes());
        let _ = stream.flush();
    }
}

impl Drop for ScriptedOpenAiEndpoint {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::SeqCst);
        let _ = TcpStream::connect(self.address);
        if let Some(worker) = self.accept_worker.take() {
            let _ = worker.join();
        }
    }
}

pub(super) fn read_scripted_http_request_body(stream: &mut TcpStream) -> std::io::Result<Vec<u8>> {
    let mut headers = Vec::new();
    let mut byte = [0_u8; 1];
    loop {
        stream.read_exact(&mut byte)?;
        headers.push(byte[0]);
        if headers.ends_with(b"\r\n\r\n") {
            break;
        }
        if headers.len() > 64 * 1024 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "scripted model request headers were too large",
            ));
        }
    }
    let headers = String::from_utf8_lossy(&headers);
    let content_length = headers
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse::<usize>().ok())
                .flatten()
        })
        .ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "scripted model request omitted content length",
            )
        })?;
    let mut body = vec![0; content_length];
    stream.read_exact(&mut body)?;
    Ok(body)
}

pub(super) fn write_scripted_http_response(mut stream: TcpStream, body: serde_json::Value) {
    let body = serde_json::to_vec(&body).unwrap();
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(response.as_bytes()).unwrap();
    stream.write_all(&body).unwrap();
    stream.flush().unwrap();
}

pub(super) fn scripted_project_provider_registry(
    endpoint: &str,
    model_id: ModelId,
) -> ProviderRegistry {
    let provider_id = ProviderId::new("scripted-project");
    let descriptor = ModelDescriptor {
        id: model_id,
        provider: provider_id.clone(),
        display_name: "Scripted project coordination model".to_owned(),
        context_window: Some(16_384),
        max_input_tokens: None,
        max_output_tokens: None,
        capabilities: ModelCapabilities {
            streaming: false,
            tool_calling: true,
            vision: false,
            json_mode: false,
        },
    };
    let providers = ProviderRegistry::new();
    providers
        .register(ProviderConfig::openai_compatible(
            provider_id,
            "Scripted project model",
            endpoint,
            descriptor,
            None,
        ))
        .unwrap();
    providers
}

pub(super) fn scripted_request_is_manager(request: &serde_json::Value) -> bool {
    request["messages"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|message| message["role"] == "system")
        .any(|message| {
            message["content"].as_str().is_some_and(|content| {
                content.contains("You are the project manager for this project")
            })
        })
}

pub(super) fn request_has_tool(request: &serde_json::Value, name: &str) -> bool {
    request["tools"]
        .as_array()
        .into_iter()
        .flatten()
        .any(|tool| tool["function"]["name"] == name)
}

pub(super) fn request_has_project_message(request: &serde_json::Value, kind: &str) -> bool {
    request["messages"]
        .as_array()
        .into_iter()
        .flatten()
        .any(|message| {
            message["name"] == "loom_project_message"
                && message["content"]
                    .as_str()
                    .is_some_and(|content| content.contains(&format!("({kind}")))
        })
}

#[allow(dead_code)]
pub(super) fn _keep_tool_id_in_scope(_: ToolCallId) {}
