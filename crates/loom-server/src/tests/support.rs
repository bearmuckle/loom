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

/// One scripted page of the GitHub repository listing fixture.
pub(super) struct GitHubListingPage {
    status: &'static str,
    headers: Vec<(&'static str, String)>,
    body: String,
    delay: Duration,
}

impl GitHubListingPage {
    pub(super) fn ok(repositories: &[serde_json::Value]) -> Self {
        Self {
            status: "200 OK",
            headers: Vec::new(),
            body: serde_json::Value::Array(repositories.to_vec()).to_string(),
            delay: Duration::ZERO,
        }
    }

    /// A failing page, with the headers GitHub sends alongside a rate limit.
    pub(super) fn failure(status: &'static str, headers: &[(&'static str, String)]) -> Self {
        Self {
            status,
            headers: headers.to_vec(),
            body: "{}".to_owned(),
            delay: Duration::ZERO,
        }
    }

    pub(super) fn with_delay(mut self, delay: Duration) -> Self {
        self.delay = delay;
        self
    }
}

/// One request the GitHub repository listing fixture received.
pub(super) struct GitHubListingRequest {
    pub(super) page: usize,
    /// The credential the request carried, without its `Bearer ` prefix.
    pub(super) token: String,
}

/// A scripted `GET /user/repos` endpoint that serves pages by number, counts
/// the requests it receives, and records which credential each one used.
///
/// Each connection serves exactly one request and is handled on its own
/// thread, so a client that fetches pages concurrently is served concurrently.
/// A page past the scripted list is an empty page, which ends a listing walk.
pub(super) struct GitHubListingFixture {
    address: std::net::SocketAddr,
    requests: Arc<Mutex<Vec<GitHubListingRequest>>>,
    stopped: Arc<AtomicBool>,
    worker: Option<thread::JoinHandle<()>>,
}

impl GitHubListingFixture {
    pub(super) fn start(pages: Vec<GitHubListingPage>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let address = listener.local_addr().unwrap();
        let requests: Arc<Mutex<Vec<GitHubListingRequest>>> = Arc::new(Mutex::new(Vec::new()));
        let stopped = Arc::new(AtomicBool::new(false));
        let worker_requests = Arc::clone(&requests);
        let worker_stopped = Arc::clone(&stopped);
        let pages = Arc::new(pages);
        let worker = thread::spawn(move || {
            while !worker_stopped.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        let requests = Arc::clone(&worker_requests);
                        let pages = Arc::clone(&pages);
                        thread::spawn(move || {
                            serve_github_listing_request(stream, &pages, &requests);
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
            address,
            requests,
            stopped,
            worker: Some(worker),
        }
    }

    pub(super) fn endpoint(&self) -> String {
        format!("http://{}/user/repos", self.address)
    }

    pub(super) fn requests(&self) -> Vec<GitHubListingRequest> {
        self.requests
            .lock()
            .unwrap()
            .iter()
            .map(|request| GitHubListingRequest {
                page: request.page,
                token: request.token.clone(),
            })
            .collect()
    }

    pub(super) fn requested_pages(&self) -> Vec<usize> {
        self.requests
            .lock()
            .unwrap()
            .iter()
            .map(|request| request.page)
            .collect()
    }

    pub(super) fn request_count(&self) -> usize {
        self.requests.lock().unwrap().len()
    }
}

impl Drop for GitHubListingFixture {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::SeqCst);
        let _ = TcpStream::connect(self.address);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn serve_github_listing_request(
    mut stream: TcpStream,
    pages: &Arc<Vec<GitHubListingPage>>,
    requests: &Arc<Mutex<Vec<GitHubListingRequest>>>,
) {
    let mut headers = Vec::new();
    let mut byte = [0_u8; 1];
    loop {
        if stream.read_exact(&mut byte).is_err() {
            return;
        }
        headers.push(byte[0]);
        if headers.ends_with(b"\r\n\r\n") {
            break;
        }
        if headers.len() > 16 * 1024 {
            return;
        }
    }
    let headers = String::from_utf8_lossy(&headers).to_string();
    let page = github_listing_page_number(&headers);
    let token = headers
        .lines()
        .filter_map(|line| line.split_once(':'))
        .find(|(name, _)| name.eq_ignore_ascii_case("authorization"))
        .map(|(_, value)| {
            value
                .trim()
                .strip_prefix("Bearer ")
                .or_else(|| value.trim().strip_prefix("bearer "))
                .unwrap_or(value.trim())
                .to_owned()
        })
        .unwrap_or_default();
    requests
        .lock()
        .unwrap()
        .push(GitHubListingRequest { page, token });

    let empty = GitHubListingPage::ok(&[]);
    let response = pages.get(page.saturating_sub(1)).unwrap_or(&empty);
    if !response.delay.is_zero() {
        thread::sleep(response.delay);
    }
    let mut head = format!(
        "HTTP/1.1 {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n",
        response.status,
        response.body.len()
    );
    for (name, value) in &response.headers {
        head.push_str(name);
        head.push_str(": ");
        head.push_str(value);
        head.push_str("\r\n");
    }
    head.push_str("\r\n");
    let _ = stream.write_all(head.as_bytes());
    let _ = stream.write_all(response.body.as_bytes());
    let _ = stream.flush();
}

/// Reads the `page` query parameter from a fixture request's request line.
fn github_listing_page_number(headers: &str) -> usize {
    headers
        .lines()
        .next()
        .and_then(|request_line| request_line.split_whitespace().nth(1))
        .and_then(|target| target.split_once('?').map(|(_, query)| query))
        .and_then(|query| query.split('&').find_map(|pair| pair.strip_prefix("page=")))
        .and_then(|page| page.parse().ok())
        .unwrap_or(1)
}

/// Builds one listing page whose first `matching` repositories contain the
/// query a test searches for, followed by `filler` repositories that do not.
pub(super) fn github_repository_page(
    prefix: &str,
    matching: usize,
    filler: usize,
) -> Vec<serde_json::Value> {
    let mut repositories = Vec::new();
    for index in 0..matching {
        repositories.push(github_repository_json(&format!(
            "{prefix}/the-needle-{index:03}"
        )));
    }
    for index in 0..filler {
        repositories.push(github_repository_json(&format!(
            "{prefix}/other-{index:03}"
        )));
    }
    repositories
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

    /// Persists a run of this child's session in `state`, so a test can list a
    /// child that has started and then stopped at that state without driving a
    /// model through it.
    pub(super) fn persist_child_run_state(&self, run_id: loom_core::RunId, state: AgentRunState) {
        self.persist_child_run_with_id(run_id, ParkedChildRun::new(state));
    }

    /// Persists one run of this child's session together with the state a test
    /// describes, and returns its id.
    pub(super) fn persist_child_run(&self, run: ParkedChildRun) -> loom_core::RunId {
        self.persist_child_run_with_id(loom_core::RunId::new(), run)
    }

    /// Persists one run of this child's session under `run_id`.
    pub(super) fn persist_child_run_with_id(
        &self,
        run_id: loom_core::RunId,
        run: ParkedChildRun,
    ) -> loom_core::RunId {
        let persistence = self.backend.persistence.as_ref().unwrap();
        let sessions = persistence.load_sessions().unwrap().unwrap();
        let session_id = self.worktree.child_session_id;
        let started_at = Timestamp::from_unix_millis(1000);
        // A run this helper already wrote keeps its attempt rows: the attempt
        // table is keyed by run and attempt number and the execution state
        // refers to the attempt row, so a second state for the same run reuses
        // that attempt instead of colliding with it.
        let existing_attempts = persistence.load_run_attempts(run_id).unwrap_or_default();
        let attempt_id = existing_attempts
            .last()
            .map(|attempt| attempt.id)
            .unwrap_or_default();
        let terminal = matches!(
            run.state,
            AgentRunState::Completed | AgentRunState::Failed | AgentRunState::Cancelled
        );
        // The attempt history ends at the current attempt, whose state and
        // completion match the summary the run is persisted with.
        let attempts = existing_attempts.is_empty().then(|| {
            (1..=run.attempts.max(1))
                .map(|number| {
                    let current = number == run.attempts.max(1);
                    loom_model::AgentRunAttemptRecord {
                        run_id,
                        session_id,
                        id: if current {
                            attempt_id
                        } else {
                            loom_core::RunAttemptId::new()
                        },
                        number,
                        state: if current {
                            run.state
                        } else {
                            AgentRunState::Failed
                        },
                        checkpoint_id: None,
                        started_at,
                        completed_at: if !current || terminal {
                            Some(started_at)
                        } else {
                            None
                        },
                    }
                })
                .collect::<Vec<_>>()
        });
        let execution = AgentExecutionStateRecord {
            run_id,
            session_id,
            attempt_id,
            control_revision: 0,
            state: run.state,
            step_id: None,
            step_index: 0,
            provider_cursor: 0,
            next_message_id: 0,
            active_message_id: None,
            last_project_message_sequence: 0,
            last_queued_direction_sequence: 0,
            pending_tool_execution: None,
            pending_project_join: None,
            pending_approval: run.pending_approval.clone(),
            pending_input: run.pending_input.clone(),
            last_failed_call: run.last_failed_call.clone(),
        };
        let interactions = run.pending_question.then(|| {
            vec![loom_model::AgentInteractionRecord {
                id: loom_core::InteractionId::new(),
                run_id,
                session_id,
                attempt_id,
                control_revision: 0,
                kind: loom_model::AgentInteractionKind::UserInput,
                status: loom_model::AgentInteractionStatus::Pending,
                tool_call_id: None,
                prompt: "Which option should I use?".to_owned(),
                decision: None,
                created_at: started_at,
                resolved_at: None,
            }]
        });
        let summary = DurableRunSummary {
            snapshot: AgentRunSnapshot {
                id: run_id,
                attempt_id,
                control_revision: 0,
                session_id,
                task: "Make a committed code change".to_owned(),
                jev: None,
                model: ModelId::new("deterministic/demo"),
                state: run.state,
                started_at,
                updated_at: started_at,
                completed_at: terminal.then_some(started_at),
                summary: run.summary,
                evidence: Vec::new(),
                approval_policy: None,
            },
            usage: run.usage,
            attempts,
            execution_state: Some(execution),
            interactions,
        };
        let summaries = BTreeMap::from([(run_id, summary)]);
        persistence
            .save_state(DurableStateWrite {
                sessions: &sessions,
                workspaces: None,
                settings: None,
                workspace_configs: None,
                providers: None,
                usage: None,
                idempotency: None,
                run_summaries: Some(&summaries),
                run_runtime_configs: None,
                run_context_checkpoints: None,
                run_plans: None,
                run_messages: None,
                run_activities: None,
                filesystem_records: None,
                feed: None,
            })
            .unwrap();
        run_id
    }
}

/// One run of a fixture child, as a test describes the state it stopped in.
pub(super) struct ParkedChildRun {
    pub(super) state: AgentRunState,
    pub(super) summary: Option<String>,
    pub(super) usage: UsageSnapshot,
    pub(super) pending_approval: Option<ToolCall>,
    pub(super) pending_input: Option<String>,
    /// Whether the run opened the user-input interaction a person has to answer.
    pub(super) pending_question: bool,
    pub(super) last_failed_call: Option<ToolCall>,
    /// How many attempts the run has been tried for.
    pub(super) attempts: u32,
}

impl ParkedChildRun {
    pub(super) fn new(state: AgentRunState) -> Self {
        Self {
            state,
            summary: None,
            usage: UsageSnapshot::default(),
            pending_approval: None,
            pending_input: None,
            pending_question: false,
            last_failed_call: None,
            attempts: 1,
        }
    }
}

pub(super) fn git_in(dir: &Path, arguments: &[&str]) {
    git_output(dir, arguments);
}

/// Runs `git` in `dir` and returns its trimmed stdout, so a test can read back
/// a branch name or a revision from a repository it just changed.
pub(super) fn git_output(dir: &Path, arguments: &[&str]) -> String {
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
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

pub(super) fn commit_file(dir: &Path, file: &str, content: &str, message: &str) {
    if let Some(parent) = Path::new(file).parent() {
        fs::create_dir_all(dir.join(parent)).unwrap();
    }
    fs::write(dir.join(file), content).unwrap();
    git_in(dir, &["config", "user.name", "Loom Test"]);
    git_in(dir, &["config", "user.email", "loom@example.test"]);
    git_in(dir, &["add", "--", file]);
    git_in(dir, &["commit", "-qm", message]);
}

pub(super) fn project_code_child_fixture() -> ProjectCodeChildFixture {
    let mut fixture = pending_project_code_child_fixture();
    create_project_code_child_checkout(&mut fixture);
    fixture
}

/// Persists a code child and its worktree intent without creating the checkout,
/// so a test can change the parent checkout's remote before the base is
/// refreshed and the linked worktree is materialized.
pub(super) fn pending_project_code_child_fixture() -> ProjectCodeChildFixture {
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
    let worktree = ProjectWorktreeRecord {
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
        landed_by: None,
        landed_evidence: None,
        scope: None,
        scope_override: None,
        rebase: None,
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

    ProjectCodeChildFixture {
        temp,
        source,
        backend,
        connection,
        root_id: root.id,
        project_id: task.project_id,
        task_id,
        worktree,
        child_checkout: PathBuf::new(),
        parent_git,
    }
}

/// Creates the checkout of a pending code child fixture: the step that
/// refreshes the recorded base against the parent checkout's `origin` before
/// the linked worktree exists.
pub(super) fn create_project_code_child_checkout(fixture: &mut ProjectCodeChildFixture) {
    fixture
        .connection
        .ensure_project_worktree_ready(&mut fixture.worktree)
        .unwrap();
    fixture.worktree = fixture
        .backend
        .persistence
        .as_ref()
        .unwrap()
        .load_project_worktree_by_task(fixture.task_id)
        .unwrap()
        .expect("child worktree should be durable after creation");
    fixture.child_checkout = fixture
        .connection
        .session_filesystem(fixture.worktree.child_session_id)
        .unwrap()
        .root()
        .join(&fixture.worktree.relative_path);
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

/// Builds the server-owned completion guard a manager consults before reporting
/// completion, with every project capability disabled so tests exercise only
/// the blocker evaluation.
pub(super) fn project_completion_guard(fixture: &ProjectCodeChildFixture) -> ProjectAgentTools {
    ProjectAgentTools {
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
    }
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
                scope: None,
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
        // Use a non-retryable status so the failed prerequisite settles
        // promptly. A retryable 5xx would be retried by the provider layer,
        // and this scripted endpoint only serves one response per turn.
        // Retryable-5xx retry behavior is covered in the provider tests.
        model.respond_with_failure(prerequisite_turn, 400, "scripted prerequisite failure");
        None
    } else {
        let stream = hold_scripted_model_stream_until_cancelled(prerequisite_turn);
        let cancel = connection.request(RequestEnvelope::new(ClientRequest::Project(
            ProjectRequest::ControlProjectChild {
                project_id,
                manager_session_id: root,
                task_id: prerequisite_task.task_id,
                action: loom_protocol::ProjectChildControlAction::Cancel,
                onto: None,
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
            onto: None,
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
    /// The request's `Authorization` header, so a test can tell which
    /// credential a provider call authenticated with.
    pub(super) authorization: Option<String>,
    /// The request's `User-Agent` header, so a test can tell what the worker
    /// told the gateway it is.
    pub(super) user_agent: Option<String>,
    /// The request's `x-opencode-session` header, so a test can tell which Loom
    /// session the gateway was asked to keep as one conversation.
    pub(super) opencode_session: Option<String>,
    pub(super) stream: TcpStream,
}

/// One scripted HTTP request: the headers a test inspects and the raw body.
struct ScriptedHttpRequest {
    authorization: Option<String>,
    user_agent: Option<String>,
    opencode_session: Option<String>,
    body: Vec<u8>,
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
                            let Ok(request) = read_scripted_http_request(&mut stream) else {
                                return;
                            };
                            let Ok(body) = serde_json::from_slice(&request.body) else {
                                return;
                            };
                            let _ = sender.send(ScriptedModelRequest {
                                request: body,
                                authorization: request.authorization,
                                user_agent: request.user_agent,
                                opencode_session: request.opencode_session,
                                stream,
                            });
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

fn read_scripted_http_request(stream: &mut TcpStream) -> std::io::Result<ScriptedHttpRequest> {
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
    let header_value = |name: &str| {
        headers.lines().find_map(|line| {
            let (header_name, value) = line.split_once(':')?;
            header_name
                .eq_ignore_ascii_case(name)
                .then(|| value.trim().to_owned())
        })
    };
    let authorization = header_value("authorization");
    let user_agent = header_value("user-agent");
    let opencode_session = header_value("x-opencode-session");
    let content_length = header_value("content-length")
        .and_then(|value| value.parse::<usize>().ok())
        .ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "scripted model request omitted content length",
            )
        })?;
    let mut body = vec![0; content_length];
    stream.read_exact(&mut body)?;
    Ok(ScriptedHttpRequest {
        authorization,
        user_agent,
        opencode_session,
        body,
    })
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
