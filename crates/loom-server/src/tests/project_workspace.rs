//! In-process tests: the workspace a project's delegated sessions share.

#[allow(unused_imports)]
use super::support::*;
use super::*;

/// Delegates one read-only child of `root` and returns its task and session.
fn delegate(
    connection: &InProcessConnection,
    root: AgentSessionId,
    child_name: &str,
) -> (loom_core::TaskId, AgentSessionId) {
    let response = connection
        .create_project_child(
            RequestId::new(),
            root,
            child_name.to_owned(),
            DelegatedTaskSpec {
                intent: format!("bounded work for {child_name}"),
                model_id: "deterministic/demo".to_owned(),
                context_references: Vec::new(),
                dependencies: Vec::new(),
                code_change: false,
                permissions: loom_core::ProjectAgentPermissions::default(),
                scope: None,
            },
        )
        .unwrap();
    match response {
        ServerResponse::Project(ProjectResponse::ProjectChildCreated { task, child }) => {
            (task.task_id, child.session_id)
        }
        response => panic!("unexpected child response: {response:?}"),
    }
}

fn create_workspace_and_root(
    connection: &InProcessConnection,
    name: &str,
) -> (WorkspaceId, AgentSessionId) {
    let workspace_id = match connection
        .request(RequestEnvelope::new(ClientRequest::Workspace(
            WorkspaceRequest::CreateWorkspace {
                name: name.to_owned(),
            },
        )))
        .result
        .unwrap()
    {
        ServerResponse::Workspace(WorkspaceResponse::WorkspaceCreated(workspace)) => workspace.id,
        response => panic!("unexpected workspace response: {response:?}"),
    };
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
    (workspace_id, root)
}

fn write_file(
    connection: &InProcessConnection,
    session_id: AgentSessionId,
    path: &str,
    content: &str,
) {
    let result = connection
        .request(RequestEnvelope::new(ClientRequest::Filesystem(
            FilesystemRequest::ApplySessionFilesystemEdit {
                session_id,
                edit: WorkspaceEdit {
                    path: path.to_owned(),
                    old_text: String::new(),
                    new_text: content.to_owned(),
                    expected_revision: None,
                },
            },
        )))
        .result;
    assert!(
        matches!(
            result,
            Ok(ServerResponse::Filesystem(
                FilesystemResponse::WorkspaceEditApplied(_)
            ))
        ),
        "unexpected edit result: {result:?}"
    );
}

fn read_file(connection: &InProcessConnection, session_id: AgentSessionId, path: &str) -> String {
    let result = connection
        .request(RequestEnvelope::new(ClientRequest::Filesystem(
            FilesystemRequest::ReadSessionFile {
                session_id,
                path: path.to_owned(),
            },
        )))
        .result;
    match result {
        Ok(ServerResponse::Filesystem(FilesystemResponse::SessionFilesystemFile(file))) => {
            file.content
        }
        result => panic!("unexpected read result: {result:?}"),
    }
}

fn attach_directory(
    connection: &InProcessConnection,
    session_id: AgentSessionId,
    source: &Path,
    path: &str,
) {
    let attached = connection.request(RequestEnvelope::new(ClientRequest::Filesystem(
        FilesystemRequest::AttachSessionDirectory {
            session_id,
            source: source.display().to_string(),
            path: path.to_owned(),
        },
    )));
    let Ok(ServerResponse::Filesystem(FilesystemResponse::SessionDirectoryAttached {
        repositories,
        ..
    })) = attached.result
    else {
        panic!("expected the plain directory to attach");
    };
    assert!(
        repositories.is_empty(),
        "a plain directory attaches no repository"
    );
}

fn detach_directory(connection: &InProcessConnection, session_id: AgentSessionId, path: &str) {
    let detached = connection.request(RequestEnvelope::new(ClientRequest::Filesystem(
        FilesystemRequest::DetachSessionDirectory {
            session_id,
            path: path.to_owned(),
        },
    )));
    assert!(
        detached.result.is_ok(),
        "unexpected detach result: {:?}",
        detached.result
    );
}

/// Waits until a delegated task is terminal, so its child session is no longer
/// active when a test asks to attach a directory to it.
fn await_terminal_task(backend: &InProcessBackend, task_id: loom_core::TaskId) {
    let persistence = backend.persistence.as_ref().unwrap();
    for _ in 0..2_000 {
        if let Some(task) = persistence.load_delegated_task(task_id).unwrap()
            && matches!(
                task.status,
                loom_core::DelegatedTaskStatus::Completed
                    | loom_core::DelegatedTaskStatus::Failed
                    | loom_core::DelegatedTaskStatus::Cancelled
            )
        {
            return;
        }
        thread::sleep(Duration::from_millis(5));
    }
    panic!("delegated task {task_id} did not reach a terminal state");
}

/// A project that keeps no Git checkout: a workspace, a project root, and one
/// attached plain directory.
struct DirectoryProject {
    temp: PathBuf,
    source: PathBuf,
    backend: Arc<InProcessBackend>,
    connection: InProcessConnection,
    workspace_id: WorkspaceId,
    root: AgentSessionId,
}

impl DirectoryProject {
    fn new(name: &str) -> Self {
        let temp = std::env::temp_dir().join(format!("loom-{name}-{}", AgentSessionId::new()));
        fs::create_dir_all(&temp).unwrap();
        let source = temp.join("attached");
        fs::create_dir_all(&source).unwrap();
        fs::write(source.join("note.txt"), "attached\n").unwrap();
        let backend = InProcessBackend::new_persistent(temp.join("state.sqlite")).unwrap();
        let connection = backend.connect();
        negotiate(&connection);
        let (workspace_id, root) = create_workspace_and_root(&connection, name);
        attach_directory(&connection, root, &source, "attached");
        Self {
            temp,
            source,
            backend,
            connection,
            workspace_id,
            root,
        }
    }

    fn delegate(&self, child_name: &str) -> (loom_core::TaskId, AgentSessionId) {
        delegate(&self.connection, self.root, child_name)
    }

    fn workspace_root(&self, session_id: AgentSessionId) -> PathBuf {
        self.connection
            .session_filesystem(session_id)
            .unwrap()
            .root()
            .to_path_buf()
    }

    fn shutdown(self) {
        self.backend.shutdown().unwrap();
        fs::remove_dir_all(&self.temp).unwrap();
    }
}

#[test]
fn delegated_siblings_read_and_write_the_project_workspace() {
    let project = DirectoryProject::new("loom-shared-project");
    let (_, coder) = project.delegate("coder");
    let (_, reviewer) = project.delegate("reviewer");

    // A project without a checkout keeps one workspace: the root's.
    let root_filesystem = project.workspace_root(project.root);
    assert_eq!(project.workspace_root(coder), root_filesystem);
    assert_eq!(project.workspace_root(reviewer), root_filesystem);
    let coder_directory = project
        .backend
        .session_root_base
        .join(project.workspace_id.to_string())
        .join(coder.to_string());
    assert!(
        !coder_directory.exists(),
        "a shared child owns no filesystem directory of its own"
    );

    // What one child writes, its sibling and the project root read.
    write_file(&project.connection, coder, "hello.py", "print('hello')\n");
    assert_eq!(
        read_file(&project.connection, reviewer, "hello.py"),
        "print('hello')\n"
    );
    assert_eq!(
        read_file(&project.connection, project.root, "hello.py"),
        "print('hello')\n"
    );

    // The project's attached directory is part of the shared workspace.
    assert_eq!(
        read_file(&project.connection, reviewer, "attached/note.txt"),
        "attached\n"
    );
    write_file(
        &project.connection,
        reviewer,
        "attached/from-reviewer.txt",
        "written\n",
    );
    assert_eq!(
        fs::read_to_string(project.source.join("from-reviewer.txt")).unwrap(),
        "written\n"
    );

    // A directory the root attaches later reaches the members that share its
    // workspace, and detaching it leaves them again.
    let later = project.temp.join("later");
    fs::create_dir_all(&later).unwrap();
    fs::write(later.join("note.txt"), "later\n").unwrap();
    attach_directory(&project.connection, project.root, &later, "later");
    assert_eq!(
        read_file(&project.connection, reviewer, "later/note.txt"),
        "later\n"
    );
    assert_eq!(
        read_file(&project.connection, coder, "later/note.txt"),
        "later\n"
    );
    detach_directory(&project.connection, project.root, "later");
    assert!(
        project
            .connection
            .session_filesystem(coder)
            .unwrap()
            .read_file("later/note.txt")
            .is_err(),
        "a detached directory leaves the shared view"
    );

    project.shutdown();
}

#[test]
fn a_shared_child_cannot_attach_a_directory_of_its_own() {
    let project = DirectoryProject::new("loom-shared-refusal");
    let (task_id, reviewer) = project.delegate("reviewer");
    await_terminal_task(&project.backend, task_id);
    let state = project
        .backend
        .sessions()
        .unwrap()
        .get(reviewer)
        .unwrap()
        .state;
    assert!(
        !matches!(
            state,
            AgentSessionState::Queued
                | AgentSessionState::Planning
                | AgentSessionState::AwaitingApproval
                | AgentSessionState::Paused
                | AgentSessionState::Executing
                | AgentSessionState::Evaluating
                | AgentSessionState::NeedsInput
        ),
        "the settled child must accept an attach request: {state:?}"
    );

    let second = project.temp.join("second");
    fs::create_dir_all(&second).unwrap();
    let attached = project
        .connection
        .request(RequestEnvelope::new(ClientRequest::Filesystem(
            FilesystemRequest::AttachSessionDirectory {
                session_id: reviewer,
                source: second.display().to_string(),
                path: "second".to_owned(),
            },
        )));
    let error = attached.result.unwrap_err();
    assert_eq!(error.code, ErrorCode::InvalidRequest);
    assert!(
        error.message.contains(&project.root.to_string()),
        "the refusal names the session that owns the workspace: {error}"
    );
    assert!(!project.workspace_root(project.root).join("second").exists());

    project.shutdown();
}

#[test]
fn a_shared_project_workspace_reopens_after_restart() {
    let project = DirectoryProject::new("loom-shared-restart");
    let (_, coder) = project.delegate("coder");
    let (_, reviewer) = project.delegate("reviewer");
    write_file(&project.connection, coder, "hello.py", "print('hello')\n");
    let database = project.temp.join("state.sqlite");
    project.backend.shutdown().unwrap();

    let backend = InProcessBackend::open_persistent(&database).unwrap();
    let connection = backend.connect();
    negotiate(&connection);
    // A cheap directory listing of a member that is not open yet resolves to
    // the directories of the root that owns the shared workspace.
    let listed = connection.request(RequestEnvelope::new(ClientRequest::Filesystem(
        FilesystemRequest::ListSessionDirectories {
            session_id: reviewer,
        },
    )));
    let Ok(ServerResponse::Filesystem(FilesystemResponse::SessionDirectories { directories })) =
        listed.result
    else {
        panic!("unexpected directory listing");
    };
    assert!(
        directories
            .iter()
            .any(|directory| directory.path == "attached"),
        "a shared member reports its project's directories: {directories:?}"
    );
    assert_eq!(
        read_file(&connection, reviewer, "hello.py"),
        "print('hello')\n"
    );
    assert_eq!(
        read_file(&connection, reviewer, "attached/note.txt"),
        "attached\n"
    );
    let restored_root = connection
        .session_filesystem(reviewer)
        .unwrap()
        .root()
        .to_path_buf();
    assert_eq!(
        connection.session_filesystem(coder).unwrap().root(),
        restored_root
    );

    backend.shutdown().unwrap();
    fs::remove_dir_all(&project.temp).unwrap();
}

#[test]
fn a_project_with_a_checkout_keeps_private_child_worktrees() {
    let temp =
        std::env::temp_dir().join(format!("loom-worktree-workspace-{}", AgentSessionId::new()));
    fs::create_dir_all(&temp).unwrap();
    let source = git_repository();
    let backend = InProcessBackend::new_persistent(temp.join("state.sqlite")).unwrap();
    let connection = backend.connect();
    negotiate(&connection);
    let (_, root) = create_workspace_and_root(&connection, "Worktree project");
    let attached = connection.request(RequestEnvelope::new(ClientRequest::Repository(
        RepositoryRequest::AttachSessionRepository {
            session_id: root,
            source: source.display().to_string(),
            path: "repo".to_owned(),
            revision: None,
            reuse_local: false,
        },
    )));
    assert!(
        matches!(
            attached.result,
            Ok(ServerResponse::Repository(
                RepositoryResponse::SessionRepositoryAttached(_)
            ))
        ),
        "unexpected repository response: {:?}",
        attached.result
    );

    let (coder_task, coder) = delegate(&connection, root, "coder");
    let (_, reviewer) = delegate(&connection, root, "reviewer");
    let root_filesystem = connection
        .session_filesystem(root)
        .unwrap()
        .root()
        .to_path_buf();
    let coder_filesystem = connection
        .session_filesystem(coder)
        .unwrap()
        .root()
        .to_path_buf();
    let reviewer_filesystem = connection
        .session_filesystem(reviewer)
        .unwrap()
        .root()
        .to_path_buf();

    // Every child keeps the private filesystem that holds its own worktree.
    assert_ne!(coder_filesystem, root_filesystem);
    assert_ne!(coder_filesystem, reviewer_filesystem);
    assert!(
        coder_filesystem
            .join(format!("project-worktrees/{coder_task}"))
            .is_dir(),
        "the code child's worktree lives in its own filesystem"
    );
    assert!(!root_filesystem.join("project-worktrees").exists());

    write_file(&connection, coder, "only-coder.txt", "private\n");
    assert!(
        connection
            .session_filesystem(reviewer)
            .unwrap()
            .read_file("only-coder.txt")
            .is_err(),
        "a worktree sibling must not see another child's checkout"
    );

    backend.shutdown().unwrap();
    fs::remove_dir_all(temp).unwrap();
    fs::remove_dir_all(source).unwrap();
}
