use std::{
    env, fs,
    io::{self, IsTerminal, Write},
    net::SocketAddr,
    path::{Path, PathBuf},
    sync::Arc,
    thread,
    time::Duration,
};

use loom_agent::{AgentEvent, AgentRunState};
use loom_core::{
    AgentSessionId, AgentSessionState, Capability, CapabilitySet, ErrorCode, LoomError, RunId,
};
use loom_model::ModelId;
use loom_process::{TaskKind, TaskSpec, TaskStatus, TerminalEvent, TerminalStatus};
use loom_protocol::{
    CURRENT_PROTOCOL_VERSION, ClientRequest, ControlRequest, ControlResponse, EventsRequest,
    EventsResponse, FilesystemRequest, FilesystemResponse, ProviderRequest, ProviderResponse,
    RepositoryRequest, RepositoryResponse, RequestEnvelope, RunRequest, RunResponse, ServerEvent,
    ServerResponse, SessionRequest, SessionResponse, TaskRequest, TaskResponse, TerminalRequest,
    TerminalResponse, WorkspaceRequest, WorkspaceResponse,
};
use loom_providers::{
    CredentialRef, FileCredentialStore, GITHUB_COPILOT_CREDENTIAL_REF, GitHubCopilotAuthenticator,
};
use loom_server::{
    AuthTokenStore, AuthorizationScope, InProcessBackend, InProcessConnection, RemoteServer,
    RemoteServerConfig, WebSocketConnection, WebSocketTransport,
};

/// How long the shell waits between event polls while a run is executing.
const EVENT_POLL_INTERVAL_MS: u64 = 20;
/// How many consecutive empty polls are tolerated before the run is treated as
/// stalled.
const MAX_IDLE_EVENT_POLLS: u32 = 1_500;

/// Ensures the state database can be opened by this build. An incompatible
/// database is wiped only when the operator asked for it (`--reset-state`) or
/// explicitly confirms the interactive prompt; otherwise the error is returned.
fn ensure_state_database(path: &Path, reset_requested: bool) -> Result<(), LoomError> {
    ensure_state_database_with(path, reset_requested, confirm_state_wipe)
}

/// The decision logic for [`ensure_state_database`] with the interactive
/// confirmation injected, so it can be exercised without a terminal.
fn ensure_state_database_with(
    path: &Path,
    reset_requested: bool,
    confirm: impl FnOnce(&Path, loom_persistence::SchemaStatus) -> Result<bool, LoomError>,
) -> Result<(), LoomError> {
    let status = loom_persistence::FilePersistence::schema_status(path)?;
    if status.is_compatible() {
        return Ok(());
    }
    let wipe = reset_requested || confirm(path, status)?;
    if !wipe {
        return Err(loom_persistence::incompatible_database_error(path, status));
    }
    loom_persistence::FilePersistence::reset_database(path)
}

fn confirm_state_wipe(
    path: &Path,
    status: loom_persistence::SchemaStatus,
) -> Result<bool, LoomError> {
    if !io::stdin().is_terminal() {
        return Ok(false);
    }
    eprintln!(
        "Loom state database '{}' uses {} and cannot be opened by this build.",
        path.display(),
        status.description()
    );
    eprint!("Wipe it and start with an empty database? [y/N] ");
    io::stderr()
        .flush()
        .map_err(|error| LoomError::new(ErrorCode::Internal, format!("{error}"), true))?;
    let mut answer = String::new();
    io::stdin()
        .read_line(&mut answer)
        .map_err(|error| LoomError::new(ErrorCode::Internal, format!("{error}"), true))?;
    Ok(matches!(
        answer.trim().to_ascii_lowercase().as_str(),
        "y" | "yes"
    ))
}

/// Initializes the `log` facade so server and agent diagnostics are visible when
/// running the native CLI, including `serve`.
fn init_logging() {
    env_logger::Builder::from_env(
        env_logger::Env::default().default_filter_or("loom_server=info,loom_agent=info"),
    )
    .format_timestamp_millis()
    .init();
}

fn main() -> Result<(), LoomError> {
    init_logging();
    let Some(options) = parse_args(env::args().skip(1))? else {
        return Ok(());
    };
    if let Some(provider) = &options.login_provider {
        return run_login(provider);
    }
    if options.serve {
        return run_server(options);
    }
    if options.m4_demo {
        return run_m4_demo(options);
    }
    let workspace_root = prepare_workspace(options.root.clone())?;
    let temporary_persistence = options.m3_demo && options.persistence.is_none();
    let persistence_path = options.persistence.clone().or_else(|| {
        options
            .m3_demo
            .then(|| env::temp_dir().join(format!("loom-m3-demo-{}.db", AgentSessionId::new())))
    });
    if let Some(path) = persistence_path.as_deref() {
        ensure_state_database(path, options.reset_state)?;
    }
    let backend = match persistence_path.as_deref() {
        Some(path) if options.model.as_str() != "deterministic/demo" => {
            InProcessBackend::new_persistent_with_github_copilot(path)?
        }
        Some(path) => InProcessBackend::new_persistent(path)?,
        None if options.model.as_str() != "deterministic/demo" => {
            InProcessBackend::new_with_github_copilot()?
        }
        None => InProcessBackend::new(),
    };
    let connection = backend.connect();

    negotiate(&connection)?;
    let workspace = create_workspace(&connection, &options.name)?;
    let session = create_session(&connection, workspace.id, &options.name)?;
    if workspace_root.join(".git").exists() {
        attach_repository(&connection, session.id, &workspace_root)?;
    }
    let run_id = start_run(&connection, session.id, &options)?;

    println!(
        "Loom native {} shell",
        if options.m3_demo { "M3 durable" } else { "M2" }
    );
    println!(
        "Connected in-process using protocol {}.{}",
        CURRENT_PROTOCOL_VERSION.major, CURRENT_PROTOCOL_VERSION.minor
    );
    println!("Workspace: {}", workspace_root.display());
    println!("Session {}: {}", session.id, session.name);
    println!("Task: {}", options.task);
    stream_run(&connection, session.id, run_id, options.manual_approval)?;
    if options.m2_demo {
        demonstrate_m2_services(&connection, session.id)?;
    }
    if options.m3_demo {
        demonstrate_m3_recovery(
            backend,
            connection,
            persistence_path.as_deref().expect("M3 persistence path"),
            temporary_persistence,
            session.id,
            run_id,
        )?;
    } else {
        backend.shutdown()?;
    }
    Ok(())
}

struct CliOptions {
    name: String,
    task: String,
    model: ModelId,
    root: Option<PathBuf>,
    manual_approval: bool,
    m2_demo: bool,
    m3_demo: bool,
    persistence: Option<PathBuf>,
    serve: bool,
    m4_demo: bool,
    bind: SocketAddr,
    token: Option<String>,
    login_provider: Option<String>,
    reset_state: bool,
    allow_insecure_remote: bool,
}

fn parse_args(mut args: impl Iterator<Item = String>) -> Result<Option<CliOptions>, LoomError> {
    let mut options = CliOptions {
        name: "M1 demo".to_owned(),
        task: "make a small repository change and validate it".to_owned(),
        model: ModelId::new("deterministic/demo"),
        root: None,
        manual_approval: false,
        m2_demo: false,
        m3_demo: false,
        persistence: None,
        serve: false,
        m4_demo: false,
        bind: "127.0.0.1:8765"
            .parse()
            .expect("valid default bind address"),
        token: None,
        login_provider: None,
        reset_state: false,
        allow_insecure_remote: false,
    };
    while let Some(argument) = args.next() {
        match argument.as_str() {
            "--name" => options.name = required_value(&mut args, "--name")?,
            "--task" => options.task = required_value(&mut args, "--task")?,
            "--model" => options.model = ModelId::new(required_value(&mut args, "--model")?),
            "--root" => options.root = Some(PathBuf::from(required_value(&mut args, "--root")?)),
            "--manual-approval" => options.manual_approval = true,
            "--m2-demo" => options.m2_demo = true,
            "--m3-demo" => options.m3_demo = true,
            "--serve" => options.serve = true,
            "--m4-demo" => options.m4_demo = true,
            "--reset-state" => options.reset_state = true,
            "--allow-insecure-remote" => options.allow_insecure_remote = true,
            "--bind" => {
                options.bind = required_value(&mut args, "--bind")?
                    .parse()
                    .map_err(|error| {
                        LoomError::invalid_request(format!(
                            "--bind must be a socket address: {error}"
                        ))
                    })?;
            }
            "--token" => options.token = Some(required_value(&mut args, "--token")?),
            "--login" => {
                options.login_provider = Some(required_value(&mut args, "--login")?);
            }
            "--persistence" => {
                options.persistence =
                    Some(PathBuf::from(required_value(&mut args, "--persistence")?));
            }
            "--help" | "-h" => {
                println!(
                    "Usage: loom [--name <name>] [--task <task>] [--model <id>] \
                     [--root <path>] [--manual-approval] [--m3-demo] \
                     [--persistence <path>] [--reset-state] \
                     [--serve --bind <addr> --token <token>] \
                     [--m4-demo] [--login github-copilot] \
                     [--allow-insecure-remote]"
                );
                println!("Add --m2-demo to exercise workspace, terminal, and task APIs.");
                println!(
                    "Add --m3-demo to persist the run, list deterministic/Ollama providers, \
                     and reopen the backend."
                );
                println!(
                    "The default workspace is an isolated directory in the system temp folder."
                );
                println!(
                    "Use --serve with an explicit bearer --token to expose the standalone \
                     WebSocket backend."
                );
                println!(
                    "A --serve --bind address that is not loopback needs --allow-insecure-remote to \
                     accept sending the bearer token in plaintext, because this shell serves \
                     plain ws:// only."
                );
                println!("Use --m4-demo to exercise a second reconnecting remote client.");
                println!("Use --login github-copilot to authenticate GitHub Copilot.");
                println!(
                    "Use --reset-state to wipe an incompatible state database instead of \
                     being prompted."
                );
                return Ok(None);
            }
            value if value.starts_with("--name=") => {
                options.name = value["--name=".len()..].to_owned();
            }
            value if value.starts_with("--task=") => {
                options.task = value["--task=".len()..].to_owned();
            }
            value if value.starts_with("--model=") => {
                options.model = ModelId::new(value["--model=".len()..].to_owned());
            }
            value if value.starts_with("--root=") => {
                options.root = Some(PathBuf::from(value["--root=".len()..].to_owned()));
            }
            value if value.starts_with("--persistence=") => {
                options.persistence =
                    Some(PathBuf::from(value["--persistence=".len()..].to_owned()));
            }
            value if value.starts_with("--bind=") => {
                options.bind = value["--bind=".len()..].parse().map_err(|error| {
                    LoomError::invalid_request(format!("--bind must be a socket address: {error}"))
                })?;
            }
            value if value.starts_with("--token=") => {
                options.token = Some(value["--token=".len()..].to_owned());
            }
            value if value.starts_with("--login=") => {
                options.login_provider = Some(value["--login=".len()..].to_owned());
            }
            _ => {
                return Err(LoomError::invalid_request(format!(
                    "unknown argument '{argument}'"
                )));
            }
        }
    }
    Ok(Some(options))
}

fn required_value(
    args: &mut impl Iterator<Item = String>,
    flag: &str,
) -> Result<String, LoomError> {
    args.next()
        .ok_or_else(|| LoomError::invalid_request(format!("{flag} requires a value")))
}

fn run_login(provider: &str) -> Result<(), LoomError> {
    if provider != "github-copilot" {
        return Err(LoomError::invalid_request(format!(
            "unsupported provider '{provider}'; supported providers: github-copilot"
        )));
    }
    let authenticator = GitHubCopilotAuthenticator::default();
    let device = authenticator.begin()?;
    println!(
        "Open {} and enter code {}.",
        device.verification_uri, device.user_code
    );
    println!(
        "Waiting for GitHub authorization (expires in {} seconds)...",
        device.expires_in
    );
    let token = authenticator.poll(&device)?;
    let credentials = FileCredentialStore::open(FileCredentialStore::default_path())?;
    credentials.insert(CredentialRef::new(GITHUB_COPILOT_CREDENTIAL_REF), token)?;
    println!("GitHub Copilot login succeeded.");
    println!(
        "Credentials saved in {}.",
        FileCredentialStore::default_path().display()
    );
    Ok(())
}

fn run_server(options: CliOptions) -> Result<(), LoomError> {
    if options.root.is_some() {
        return Err(LoomError::invalid_request(
            "--root is only available for one-shot sessions; server sessions manage their own repositories",
        ));
    }
    let token = options
        .token
        .filter(|token| !token.trim().is_empty())
        .ok_or_else(|| LoomError::invalid_request("--serve requires a non-empty --token"))?;
    let persistence_path = options.persistence;
    if let Some(path) = persistence_path.as_deref() {
        ensure_state_database(path, options.reset_state)?;
    }
    let backend = match persistence_path {
        Some(path) => InProcessBackend::new_persistent_with_github_copilot(path)?,
        None => InProcessBackend::new_with_github_copilot()?,
    };
    let auth = Arc::new(AuthTokenStore::new());
    let _issued = auth.insert(token, AuthorizationScope::all())?;
    let config = RemoteServerConfig {
        bind_addr: options.bind,
        allow_insecure_remote: options.allow_insecure_remote,
        ..RemoteServerConfig::default()
    };
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|error| {
            LoomError::new(
                ErrorCode::Internal,
                format!("could not start async runtime: {error}"),
                false,
            )
        })?;
    runtime.block_on(async move {
        let server = RemoteServer::new(backend.clone(), auth, config)
            .bind()
            .await?;
        println!(
            "Loom remote backend listening at {}",
            server.websocket_url()
        );
        println!("Health endpoint: http://{}/health", server.local_addr());
        tokio::signal::ctrl_c().await.map_err(|error| {
            LoomError::new(
                ErrorCode::Internal,
                format!("could not wait for shutdown: {error}"),
                false,
            )
        })?;
        server.stop().await?;
        backend.shutdown()
    })
}

fn run_m4_demo(options: CliOptions) -> Result<(), LoomError> {
    let workspace_root = prepare_workspace(options.root)?;
    let backend = InProcessBackend::new();
    let auth = Arc::new(AuthTokenStore::new());
    let token = "loom-m4-demo-token";
    let _issued = auth.insert(token, AuthorizationScope::all())?;
    let config = RemoteServerConfig::local_ephemeral();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|error| {
            LoomError::new(
                ErrorCode::Internal,
                format!("could not start async runtime: {error}"),
                false,
            )
        })?;
    runtime.block_on(async move {
        let server = RemoteServer::new(backend.clone(), auth, config)
            .bind()
            .await?;
        let result = m4_demo_remote(
            server.websocket_url().to_owned(),
            token.to_owned(),
            workspace_root,
            options.task,
        )
        .await;
        let stop_result = server.stop().await;
        result.and(stop_result).and_then(|()| backend.shutdown())
    })
}

async fn m4_demo_remote(
    url: String,
    token: String,
    workspace_root: PathBuf,
    task: String,
) -> Result<(), LoomError> {
    let transport = WebSocketTransport::new(url, token);
    let mut first = transport.connect().await?;
    negotiate_remote(&mut first).await?;
    let workspace = match first
        .request(RequestEnvelope::new(ClientRequest::Workspace(
            WorkspaceRequest::CreateWorkspace {
                name: "M4 remote reconnect".to_owned(),
            },
        )))
        .await?
        .result?
    {
        ServerResponse::Workspace(WorkspaceResponse::WorkspaceCreated(workspace)) => workspace,
        response => return Err(unexpected_response("remote workspace creation", response)),
    };
    let session = match first
        .request(RequestEnvelope::new(ClientRequest::Workspace(
            WorkspaceRequest::CreateAgentSessionInWorkspace {
                workspace_id: workspace.id,
                name: "M4 remote reconnect".to_owned(),
            },
        )))
        .await?
        .result?
    {
        ServerResponse::Session(SessionResponse::AgentSessionCreated(snapshot)) => snapshot,
        response => return Err(unexpected_response("remote session creation", response)),
    };
    if workspace_root.join(".git").exists() {
        match first
            .request(RequestEnvelope::new(ClientRequest::Repository(
                RepositoryRequest::AttachSessionRepository {
                    session_id: session.id,
                    source: workspace_root.display().to_string(),
                    path: "repo".to_owned(),
                    revision: None,
                    reuse_local: false,
                },
            )))
            .await?
            .result?
        {
            ServerResponse::Repository(RepositoryResponse::SessionRepositoryAttached(_)) => {}
            response => {
                return Err(unexpected_response(
                    "remote repository attachment",
                    response,
                ));
            }
        }
    }
    first
        .request(RequestEnvelope::new(ClientRequest::Session(
            SessionRequest::SetSessionApprovalPolicy {
                session_id: session.id,
                policy: loom_core::ApprovalPolicy::default(),
                auto_approve_actions: Some(false),
            },
        )))
        .await?
        .result?;
    let run_id = match first
        .request(RequestEnvelope::new(ClientRequest::Run(
            RunRequest::StartSessionAgentRun {
                session_id: session.id,
                task,
                model: ModelId::new("deterministic/demo"),
                system_instructions: Some(
                    "Use the available tools and report validation.".to_owned(),
                ),
                repository_instructions: Some("Keep the demonstration change small.".to_owned()),
            },
        )))
        .await?
        .result?
    {
        ServerResponse::Run(RunResponse::AgentRunStarted(snapshot)) => snapshot.id,
        response => return Err(unexpected_response("remote run start", response)),
    };
    println!("M4 client 1 started run {run_id}; disconnecting before approval");
    drop(first);

    let mut second = transport.connect().await?;
    negotiate_remote(&mut second).await?;
    let mut after = None;
    let mut stream_epoch = None;
    let mut completed = false;
    for _ in 0..100 {
        let response = second
            .request(RequestEnvelope::new(ClientRequest::Events(
                EventsRequest::GetSessionEvents {
                    session_id: Some(session.id),
                    workspace_id: None,
                    after_sequence: after,
                    stream_epoch: stream_epoch.clone(),
                },
            )))
            .await?;
        let events = match response.result? {
            ServerResponse::Events(EventsResponse::SessionEvents {
                events,
                stream_epoch: current_epoch,
            })
            | ServerResponse::Events(EventsResponse::SessionEventsSnapshot {
                events,
                stream_epoch: current_epoch,
                ..
            }) => {
                stream_epoch = current_epoch;
                events
            }
            response => return Err(unexpected_response("remote event resume", response)),
        };
        if events.is_empty() {
            tokio::time::sleep(Duration::from_millis(10)).await;
            continue;
        }
        for event in events {
            after = Some(event.sequence);
            if matches!(
                &event.event,
                ServerEvent::Agent {
                    event: AgentEvent::StepStarted { run_id: event_run, .. }
                } if *event_run == run_id
            ) {
                println!("M4 client 2 observed a resumed agent step");
            }
            if let ServerEvent::Agent {
                event:
                    AgentEvent::ToolApprovalRequired {
                        run_id: event_run,
                        attempt_id,
                        control_revision,
                        call,
                        ..
                    },
            } = &event.event
                && *event_run == run_id
            {
                println!("M4 client 2 approving {}", call.name);
                second
                    .request(RequestEnvelope::new(ClientRequest::Run(
                        RunRequest::ApproveAgentAction {
                            run_id,
                            attempt_id: *attempt_id,
                            expected_control_revision: *control_revision,
                            tool_call_id: call.id,
                        },
                    )))
                    .await?
                    .result?;
            }
            if matches!(
                &event.event,
                ServerEvent::Agent {
                    event: AgentEvent::RunCompleted { snapshot }
                } if snapshot.id == run_id
            ) {
                completed = true;
            }
        }
        if completed {
            break;
        }
    }
    let final_run = second
        .request(RequestEnvelope::new(ClientRequest::Run(
            RunRequest::GetAgentRun { run_id },
        )))
        .await?;
    let ServerResponse::Run(RunResponse::AgentRun(snapshot)) = final_run.result? else {
        return Err(LoomError::new(
            ErrorCode::Internal,
            "remote client received an unexpected final run response",
            false,
        ));
    };
    println!(
        "M4 client 2 retrieved result: {} [{}]",
        snapshot.summary.unwrap_or_else(|| "none".to_owned()),
        run_state_name(snapshot.state)
    );
    second.close().await?;
    Ok(())
}

async fn negotiate_remote(connection: &mut WebSocketConnection) -> Result<(), LoomError> {
    let response = connection
        .request(RequestEnvelope::new(ClientRequest::Control(
            ControlRequest::DiscoverCapabilities,
        )))
        .await?;
    if !matches!(
        response.result?,
        ServerResponse::Control(ControlResponse::Capabilities(_))
    ) {
        return Err(LoomError::new(
            ErrorCode::UnsupportedProtocol,
            "remote server did not return capability discovery",
            false,
        ));
    }
    let response = connection
        .request(RequestEnvelope::new(ClientRequest::Control(
            ControlRequest::Negotiate {
                client_version: CURRENT_PROTOCOL_VERSION,
                capabilities: client_capabilities(),
            },
        )))
        .await?;
    match response.result? {
        ServerResponse::Control(ControlResponse::Negotiated(_)) => Ok(()),
        response => Err(unexpected_response("remote negotiation", response)),
    }
}

fn client_capabilities() -> CapabilitySet {
    CapabilitySet::new([
        Capability::CreateAgentSession,
        Capability::ReadAgentSession,
        Capability::ManageWorkspaces,
        Capability::ManageSessionRepositories,
        Capability::ReadSessionFilesystem,
        Capability::WriteSessionFilesystem,
        Capability::SubscribeSessionEvents,
        Capability::StartAgentRun,
        Capability::ReadAgentRun,
        Capability::ControlAgentRun,
        Capability::PauseAgentRun,
        Capability::ResumeAgentRun,
        Capability::ForkAgentSession,
        Capability::RetryFromCheckpoint,
        Capability::ApproveAgentAction,
        Capability::ListProviders,
        Capability::ConfigureProviders,
        Capability::ReadProviderHealth,
        Capability::ReadUsage,
        Capability::InspectContext,
        Capability::ReadWorkspaceConfig,
        Capability::OpenSessionTerminal,
        Capability::ControlSessionTerminal,
        Capability::ReadSessionTask,
        Capability::StartSessionTask,
        Capability::ControlSessionTask,
        Capability::ConfigureApprovalPolicy,
        Capability::ManageCheckpoints,
        Capability::ReadVcsStatus,
        Capability::ReadVcsDiff,
        Capability::ReadSessionTaskEvidence,
        Capability::JsonProtocol,
    ])
}

fn demonstrate_m2_services(
    connection: &InProcessConnection,
    session_id: AgentSessionId,
) -> Result<(), LoomError> {
    let filesystem = connection.request(RequestEnvelope::new(ClientRequest::Filesystem(
        FilesystemRequest::GetSessionFilesystemSnapshot { session_id },
    )));
    let snapshot = match filesystem.result? {
        ServerResponse::Filesystem(FilesystemResponse::SessionFilesystemSnapshot(snapshot)) => {
            snapshot
        }
        response => return Err(unexpected_response("workspace open", response)),
    };
    println!(
        "M2 workspace snapshot: {} entries under {}",
        snapshot.entries.len(),
        snapshot.root
    );

    let (command, args) = if cfg!(windows) {
        (
            "cmd".to_owned(),
            vec!["/C".to_owned(), "echo terminal".to_owned()],
        )
    } else {
        ("printf".to_owned(), vec!["terminal\\n".to_owned()])
    };
    let terminal = match connection
        .request(RequestEnvelope::new(ClientRequest::Terminal(
            TerminalRequest::OpenSessionTerminal {
                session_id,
                command,
                args,
                cwd: None,
            },
        )))
        .result?
    {
        ServerResponse::Terminal(TerminalResponse::TerminalOpened(snapshot)) => snapshot,
        response => return Err(unexpected_response("terminal open", response)),
    };
    connection
        .request(RequestEnvelope::new(ClientRequest::Terminal(
            TerminalRequest::ResizeSessionTerminal {
                session_id,
                terminal_id: terminal.id,
                rows: 30,
                columns: 100,
            },
        )))
        .result?;
    let mut after_terminal = None;
    for _ in 0..100 {
        let events = match connection
            .request(RequestEnvelope::new(ClientRequest::Terminal(
                TerminalRequest::GetSessionTerminalEvents {
                    session_id,
                    terminal_id: terminal.id,
                    after_sequence: after_terminal,
                },
            )))
            .result?
        {
            ServerResponse::Terminal(TerminalResponse::TerminalEvents { events }) => events,
            response => return Err(unexpected_response("terminal events", response)),
        };
        let finished = events.iter().any(|event| {
            matches!(
                event.event,
                TerminalEvent::Exited {
                    status: TerminalStatus::Exited
                        | TerminalStatus::Failed
                        | TerminalStatus::Cancelled,
                    ..
                }
            )
        });
        for event in events {
            after_terminal = Some(event.sequence);
            println!("M2 terminal #{}: {:?}", event.sequence, event.event);
        }
        if finished {
            break;
        }
        thread::sleep(Duration::from_millis(10));
    }

    let (command, args) = if cfg!(windows) {
        (
            "cmd".to_owned(),
            vec!["/C".to_owned(), "echo task".to_owned()],
        )
    } else {
        ("printf".to_owned(), vec!["task\\n".to_owned()])
    };
    let task = match connection
        .request(RequestEnvelope::new(ClientRequest::Task(
            TaskRequest::StartSessionTask {
                session_id,
                spec: TaskSpec {
                    kind: TaskKind::Test,
                    label: "M2 demo task".to_owned(),
                    command,
                    args,
                    cwd: None,
                    output_limit_bytes: Some(16 * 1024),
                    artifact_paths: Vec::new(),
                },
            },
        )))
        .result?
    {
        ServerResponse::Task(TaskResponse::TaskStarted(snapshot)) => snapshot,
        response => return Err(unexpected_response("task start", response)),
    };
    for _ in 0..100 {
        let snapshot = match connection
            .request(RequestEnvelope::new(ClientRequest::Task(
                TaskRequest::GetSessionTask {
                    session_id,
                    task_id: task.id,
                },
            )))
            .result?
        {
            ServerResponse::Task(TaskResponse::Task(snapshot)) => snapshot,
            response => return Err(unexpected_response("task status", response)),
        };
        if matches!(
            snapshot.status,
            TaskStatus::Completed | TaskStatus::Failed | TaskStatus::Cancelled
        ) {
            println!(
                "M2 task {} [{:?}]: {}",
                snapshot.label,
                snapshot.status,
                snapshot.output.trim()
            );
            break;
        }
        thread::sleep(Duration::from_millis(10));
    }
    Ok(())
}

fn demonstrate_m3_recovery(
    backend: std::sync::Arc<InProcessBackend>,
    connection: InProcessConnection,
    persistence_path: &Path,
    temporary_persistence: bool,
    session_id: AgentSessionId,
    run_id: RunId,
) -> Result<(), LoomError> {
    let providers = connection.request(RequestEnvelope::new(ClientRequest::Provider(
        ProviderRequest::ListProviders,
    )));
    if let ServerResponse::Provider(ProviderResponse::Providers { providers }) = providers.result? {
        println!("M3 providers:");
        for provider in providers {
            println!(
                "  {} ({:?}) - {} model(s), health {:?}",
                provider.id.as_str(),
                provider.kind,
                provider.models.len(),
                provider.health.state
            );
        }
    }
    backend.shutdown()?;
    drop(connection);
    drop(backend);

    let recovered_backend = InProcessBackend::open_persistent(persistence_path)?;
    let recovered = recovered_backend.connect();
    negotiate(&recovered)?;
    let session = recovered.request(RequestEnvelope::new(ClientRequest::Session(
        SessionRequest::GetAgentSession { session_id },
    )));
    let session = match session.result? {
        ServerResponse::Session(SessionResponse::AgentSession(snapshot)) => snapshot,
        response => return Err(unexpected_response("recovered session", response)),
    };
    let run = recovered.request(RequestEnvelope::new(ClientRequest::Run(
        RunRequest::GetAgentRun { run_id },
    )));
    let run = match run.result? {
        ServerResponse::Run(RunResponse::AgentRun(snapshot)) => snapshot,
        response => return Err(unexpected_response("recovered run", response)),
    };
    let events = recovered.request(RequestEnvelope::new(ClientRequest::Events(
        EventsRequest::GetSessionEvents {
            session_id: Some(session_id),
            workspace_id: None,
            after_sequence: None,
            stream_epoch: None,
        },
    )));
    let event_count = match events.result? {
        ServerResponse::Events(EventsResponse::SessionEvents { events, .. }) => events.len(),
        response => return Err(unexpected_response("recovered events", response)),
    };
    let filesystem = recovered.request(RequestEnvelope::new(ClientRequest::Filesystem(
        FilesystemRequest::GetSessionFilesystemSnapshot { session_id },
    )));
    let entries = match filesystem.result? {
        ServerResponse::Filesystem(FilesystemResponse::SessionFilesystemSnapshot(snapshot)) => {
            snapshot.entries.len()
        }
        response => return Err(unexpected_response("recovered workspace", response)),
    };
    println!(
        "M3 recovered session {} [{}], run [{}], {} events, {} workspace entries",
        session.id,
        session_state_name(session.state),
        run_state_name(run.state),
        event_count,
        entries
    );
    if temporary_persistence {
        fs::remove_file(persistence_path).map_err(|error| {
            LoomError::new(
                ErrorCode::ToolExecution,
                format!(
                    "could not remove temporary M3 persistence file '{}': {error}",
                    persistence_path.display()
                ),
                false,
            )
        })?;
    }
    Ok(())
}

fn prepare_workspace(root: Option<PathBuf>) -> Result<PathBuf, LoomError> {
    let default_root = env::temp_dir().join("loom-m1-demo");
    let root = root.unwrap_or_else(|| default_root.clone());
    fs::create_dir_all(&root).map_err(|error| {
        LoomError::new(
            ErrorCode::ToolExecution,
            format!("could not create workspace '{}': {error}", root.display()),
            false,
        )
    })?;
    if root.ends_with("loom-m1-demo") {
        let readme = root.join("README.md");
        if !readme.exists() {
            fs::write(
                &readme,
                "Workspace used by the Loom M1 deterministic demo.\n",
            )
            .map_err(|error| {
                LoomError::new(
                    ErrorCode::ToolExecution,
                    format!("could not seed demo workspace: {error}"),
                    false,
                )
            })?;
        }
        if root == default_root {
            let demo_file = root.join("loom-m1-demo.txt");
            if demo_file.exists() {
                fs::remove_file(demo_file).map_err(|error| {
                    LoomError::new(
                        ErrorCode::ToolExecution,
                        format!("could not reset demo workspace: {error}"),
                        false,
                    )
                })?;
            }
        }
    }
    Ok(root)
}

fn negotiate(connection: &InProcessConnection) -> Result<(), LoomError> {
    let response = connection.request(RequestEnvelope::new(ClientRequest::Control(
        ControlRequest::Negotiate {
            client_version: CURRENT_PROTOCOL_VERSION,
            capabilities: CapabilitySet::new([
                Capability::CreateAgentSession,
                Capability::ReadAgentSession,
                Capability::ManageWorkspaces,
                Capability::ManageSessionRepositories,
                Capability::ReadSessionFilesystem,
                Capability::WriteSessionFilesystem,
                Capability::SubscribeSessionEvents,
                Capability::StartAgentRun,
                Capability::ReadAgentRun,
                Capability::ControlAgentRun,
                Capability::PauseAgentRun,
                Capability::ResumeAgentRun,
                Capability::ForkAgentSession,
                Capability::RetryFromCheckpoint,
                Capability::ApproveAgentAction,
                Capability::ListProviders,
                Capability::ConfigureProviders,
                Capability::ReadProviderHealth,
                Capability::ReadUsage,
                Capability::InspectContext,
                Capability::ReadWorkspaceConfig,
                Capability::OpenSessionTerminal,
                Capability::ControlSessionTerminal,
                Capability::ReadSessionTask,
                Capability::StartSessionTask,
                Capability::ControlSessionTask,
                Capability::ConfigureApprovalPolicy,
                Capability::ManageCheckpoints,
                Capability::ReadVcsStatus,
                Capability::ReadVcsDiff,
                Capability::ReadSessionTaskEvidence,
                Capability::JsonProtocol,
            ]),
        },
    )));
    match response.result? {
        ServerResponse::Control(ControlResponse::Negotiated(_)) => Ok(()),
        response => Err(unexpected_response("negotiation", response)),
    }
}

fn create_session(
    connection: &InProcessConnection,
    workspace_id: loom_core::WorkspaceId,
    name: &str,
) -> Result<loom_core::AgentSessionSnapshot, LoomError> {
    let response = connection.request(RequestEnvelope::new(ClientRequest::Workspace(
        WorkspaceRequest::CreateAgentSessionInWorkspace {
            workspace_id,
            name: name.to_owned(),
        },
    )));
    match response.result? {
        ServerResponse::Session(SessionResponse::AgentSessionCreated(snapshot)) => Ok(snapshot),
        response => Err(unexpected_response("session creation", response)),
    }
}

fn create_workspace(
    connection: &InProcessConnection,
    name: &str,
) -> Result<loom_core::WorkspaceRecord, LoomError> {
    let response = connection.request(RequestEnvelope::new(ClientRequest::Workspace(
        WorkspaceRequest::CreateWorkspace {
            name: name.to_owned(),
        },
    )));
    match response.result? {
        ServerResponse::Workspace(WorkspaceResponse::WorkspaceCreated(workspace)) => Ok(workspace),
        response => Err(unexpected_response("workspace creation", response)),
    }
}

fn attach_repository(
    connection: &InProcessConnection,
    session_id: AgentSessionId,
    root: &Path,
) -> Result<(), LoomError> {
    let response = connection.request(RequestEnvelope::new(ClientRequest::Repository(
        RepositoryRequest::AttachSessionRepository {
            session_id,
            source: root.display().to_string(),
            path: "repo".to_owned(),
            revision: None,
            reuse_local: false,
        },
    )));
    match response.result? {
        ServerResponse::Repository(RepositoryResponse::SessionRepositoryAttached(_)) => Ok(()),
        response => Err(unexpected_response("repository attachment", response)),
    }
}

fn start_run(
    connection: &InProcessConnection,
    session_id: AgentSessionId,
    options: &CliOptions,
) -> Result<RunId, LoomError> {
    if options.manual_approval {
        match connection
            .request(RequestEnvelope::new(ClientRequest::Session(
                SessionRequest::SetSessionApprovalPolicy {
                    session_id,
                    policy: loom_core::ApprovalPolicy::default(),
                    auto_approve_actions: Some(false),
                },
            )))
            .result?
        {
            ServerResponse::Session(SessionResponse::ApprovalPolicy(_)) => {}
            response => return Err(unexpected_response("approval policy", response)),
        }
    }
    let response = connection.request(RequestEnvelope::new(ClientRequest::Run(
        RunRequest::StartSessionAgentRun {
            session_id,
            task: options.task.clone(),
            model: options.model.clone(),
            system_instructions: Some(
                "Work methodically, use the available tools, and report validation.".to_owned(),
            ),
            repository_instructions: Some(
                "Keep the demonstration change small and workspace-scoped.".to_owned(),
            ),
        },
    )));
    match response.result? {
        ServerResponse::Run(RunResponse::AgentRunStarted(snapshot)) => Ok(snapshot.id),
        response => Err(unexpected_response("agent run start", response)),
    }
}

fn stream_run(
    connection: &InProcessConnection,
    session_id: AgentSessionId,
    run_id: RunId,
    manual_approval: bool,
) -> Result<(), LoomError> {
    let mut after = None;
    let mut stream_epoch = None;
    // The run executes on a backend worker, so an empty poll only means the
    // current step has not journaled anything yet.
    let mut idle_polls = 0_u32;
    loop {
        let response = connection.request(RequestEnvelope::new(ClientRequest::Events(
            EventsRequest::GetSessionEvents {
                session_id: Some(session_id),
                workspace_id: None,
                after_sequence: after,
                stream_epoch: stream_epoch.clone(),
            },
        )));
        let events = match response.result? {
            ServerResponse::Events(EventsResponse::SessionEvents {
                events,
                stream_epoch: current_epoch,
            }) => {
                stream_epoch = current_epoch;
                events
            }
            response => return Err(unexpected_response("event stream", response)),
        };
        if events.is_empty() {
            idle_polls = idle_polls.saturating_add(1);
            if idle_polls > MAX_IDLE_EVENT_POLLS {
                return Err(LoomError::new(
                    ErrorCode::Internal,
                    "agent run produced no further events",
                    false,
                ));
            }
            std::thread::sleep(Duration::from_millis(EVENT_POLL_INTERVAL_MS));
            continue;
        }
        idle_polls = 0;

        let mut completed = false;
        for event in events {
            after = Some(event.sequence);
            render_event(&event);
            if let ServerEvent::Agent {
                event:
                    AgentEvent::ToolApprovalRequired {
                        run_id: event_run_id,
                        attempt_id,
                        control_revision,
                        call,
                        ..
                    },
            } = &event.event
            {
                if *event_run_id != run_id {
                    continue;
                }
                let approved = if manual_approval {
                    prompt_for_approval(&call.name)?
                } else {
                    println!("  approval: automatically approved for the demo");
                    true
                };
                let request = if approved {
                    ClientRequest::Run(RunRequest::ApproveAgentAction {
                        run_id,
                        attempt_id: *attempt_id,
                        expected_control_revision: *control_revision,
                        tool_call_id: call.id,
                    })
                } else {
                    ClientRequest::Run(RunRequest::RejectAgentAction {
                        run_id,
                        attempt_id: *attempt_id,
                        expected_control_revision: *control_revision,
                        tool_call_id: call.id,
                        reason: Some("denied at the native shell".to_owned()),
                    })
                };
                let response = connection.request(RequestEnvelope::new(request));
                response.result?;
            }
            if matches!(
                &event.event,
                ServerEvent::Agent {
                    event: AgentEvent::RunCompleted { snapshot }
                } if snapshot.id == run_id
            ) {
                completed = true;
            }
        }
        if completed {
            let response = connection.request(RequestEnvelope::new(ClientRequest::Run(
                RunRequest::GetAgentRun { run_id },
            )));
            let ServerResponse::Run(RunResponse::AgentRun(snapshot)) = response.result? else {
                return Err(LoomError::new(
                    ErrorCode::Internal,
                    "backend returned an unexpected final run response",
                    false,
                ));
            };
            let evidence = if snapshot.evidence.is_empty() {
                "none".to_owned()
            } else {
                snapshot
                    .evidence
                    .iter()
                    .map(|link| format!("{} ({})", link.label, link.uri))
                    .collect::<Vec<_>>()
                    .join(", ")
            };
            println!(
                "Final summary [{}]: {} | evidence: {}",
                run_state_name(snapshot.state),
                snapshot.summary.unwrap_or_else(|| "none".to_owned()),
                evidence
            );
            return Ok(());
        }
    }
}

fn prompt_for_approval(tool_name: &str) -> Result<bool, LoomError> {
    print!("  approve {tool_name}? [a]pprove/[d]eny: ");
    io::stdout().flush().map_err(|error| {
        LoomError::new(
            ErrorCode::Internal,
            format!("could not flush approval prompt: {error}"),
            false,
        )
    })?;
    let mut input = String::new();
    io::stdin().read_line(&mut input).map_err(|error| {
        LoomError::new(
            ErrorCode::Internal,
            format!("could not read approval decision: {error}"),
            false,
        )
    })?;
    Ok(matches!(
        input.trim().to_ascii_lowercase().as_str(),
        "a" | "approve"
    ))
}

fn render_event(envelope: &loom_protocol::ServerEventEnvelope) {
    print!("#{:<3} ", envelope.sequence);
    match &envelope.event {
        ServerEvent::ProjectTaskUpdated { task } => println!(
            "project task {} [{:?}]: {}",
            task.task_id, task.status, task.child_name
        ),
        ServerEvent::ProjectChildWorktreeUpdated { worktree } => println!(
            "project child worktree {} [{:?}]: {}",
            worktree.task_id, worktree.status, worktree.branch_name
        ),
        ServerEvent::ProjectAgentMessageAccepted { message } => println!(
            "project message {:?} {} -> {}: {}",
            message.kind,
            message.sender_session_id,
            message.target_session_id,
            message.body.lines().next().unwrap_or_default()
        ),
        ServerEvent::ProjectAgentCreated { agent } => println!(
            "project agent created: {} [depth {}, {}]",
            agent.session_id,
            agent.depth,
            session_state_name(agent.state)
        ),
        ServerEvent::ProjectAgentUpdated { agent } => println!(
            "project agent updated: {} [depth {}, {}]",
            agent.session_id,
            agent.depth,
            session_state_name(agent.state)
        ),
        ServerEvent::AgentSessionCreated { snapshot } => println!(
            "session created: {} [{}]",
            snapshot.id,
            session_state_name(snapshot.state)
        ),
        ServerEvent::AgentSessionStateChanged { current, .. } => {
            println!("      session state -> {}", session_state_name(*current));
        }
        ServerEvent::AgentSessionForked {
            source_session_id,
            snapshot,
        } => {
            println!(
                "session forked from {}: {} [{}]",
                source_session_id,
                snapshot.id,
                session_state_name(snapshot.state)
            );
        }
        ServerEvent::AgentSessionRenamed { name, .. } => {
            println!("session renamed: {name}");
        }
        ServerEvent::AgentSessionArchived { .. } => {
            println!("session archived");
        }
        ServerEvent::Agent { event } => match event {
            AgentEvent::RunStarted { snapshot } => {
                println!("Run {} [{}]", snapshot.id, run_state_name(snapshot.state));
            }
            AgentEvent::PlanProposed { plan, .. } => {
                println!("Plan:");
                for (index, step) in plan.steps.iter().enumerate() {
                    println!("  {}. {}", index + 1, step.description);
                }
            }
            AgentEvent::StepStarted { index, .. } => {
                println!("Step {} started", index + 1);
            }
            AgentEvent::StepCompleted { index, .. } => {
                println!("Step {} completed", index + 1);
            }
            AgentEvent::ContextInspected { inspection, .. } => {
                println!(
                    "Context: {} input tokens ({} omitted, compacted: {})",
                    inspection.included_tokens, inspection.omitted_tokens, inspection.compacted
                );
            }
            AgentEvent::ProviderError { error, .. } => {
                println!("Provider error [{}]: {}", error.code, error.message);
            }
            AgentEvent::ContextError { error, .. } => {
                println!("Context error [{}]: {}", error.code, error.message);
            }
            AgentEvent::AssistantMessageDelta { text, .. } => {
                println!("Assistant: {}", text.trim_end());
            }
            AgentEvent::ReasoningDelta { text, .. } => {
                println!("Reasoning: {}", text.trim_end());
            }
            AgentEvent::UserMessage { text, .. } => {
                println!("User: {}", text.trim_end());
            }
            AgentEvent::NeedsInput { prompt, .. } => {
                println!("Input required: {}", prompt.trim_end());
            }
            AgentEvent::ToolCallRequested { call, .. } => {
                println!(
                    "Tool requested: {} {}",
                    call.name,
                    serde_json::to_string(&call.arguments).unwrap_or_default()
                );
            }
            AgentEvent::ToolApprovalRequired { call, .. } => {
                println!("Approval required: {}", call.name);
            }
            AgentEvent::ToolPolicyEvaluated { evaluation, .. } => {
                println!("Policy: {:?} ({})", evaluation.decision, evaluation.reason);
            }
            AgentEvent::ToolApprovalDecided { decision, .. } => {
                println!("Approval decision: {decision:?}");
            }
            AgentEvent::ToolCallStarted { call, .. } => {
                println!("Tool started: {}", call.name);
            }
            AgentEvent::ToolOutputChunk { chunk, .. } => {
                println!("Tool output:\n{chunk}");
            }
            AgentEvent::ToolCallCompleted { result, .. } => {
                println!(
                    "Tool completed: {} [{}]",
                    result.name,
                    if result.success { "ok" } else { "failed" }
                );
            }
            AgentEvent::ActivityRecorded { .. } => {}
            AgentEvent::RunUsage { usage, .. } => println!(
                "Usage: {} input / {} output tokens",
                usage.input_tokens, usage.output_tokens
            ),
            AgentEvent::RunUsageUpdated { usage, .. } => println!(
                "Total usage: {} input / {} output tokens / {} tool calls / {} cost micros",
                usage.input_tokens, usage.output_tokens, usage.tool_calls, usage.cost_micros
            ),
            AgentEvent::RunLimitReached { status, .. } => {
                println!("Session limit reached: {:?}", status.exceeded);
            }
            AgentEvent::RecoveryRequired { reason, .. } => {
                println!("Recovery requires attention: {reason}");
            }
            AgentEvent::RunStateChanged { state, .. } => {
                println!("Run state -> {}", run_state_name(*state));
            }
            AgentEvent::RunCompleted { snapshot } => {
                println!("Run completed [{}]", run_state_name(snapshot.state))
            }
        },
        ServerEvent::SessionFilesystemChanged { change } => {
            println!("Workspace {:?}: {}", change.kind, change.path);
        }
        ServerEvent::Terminal { event } => {
            println!("Terminal event: {:?}", event.event);
        }
        ServerEvent::Task { event } => {
            println!("Task event: {:?}", event.event);
        }
        ServerEvent::ProviderHealthChanged {
            provider_id,
            health,
        } => {
            println!(
                "Provider {} health: {:?}",
                provider_id.as_str(),
                health.state
            );
        }
    }
}

fn unexpected_response(operation: &str, response: ServerResponse) -> LoomError {
    LoomError::new(
        ErrorCode::Internal,
        format!("backend returned unexpected {operation} response: {response:?}"),
        false,
    )
}

const fn session_state_name(state: AgentSessionState) -> &'static str {
    match state {
        AgentSessionState::Idle => "idle",
        AgentSessionState::Queued => "queued",
        AgentSessionState::Planning => "planning",
        AgentSessionState::AwaitingApproval => "awaiting_approval",
        AgentSessionState::Paused => "paused",
        AgentSessionState::Executing => "executing",
        AgentSessionState::Evaluating => "evaluating",
        AgentSessionState::NeedsInput => "needs_input",
        AgentSessionState::Completed => "completed",
        AgentSessionState::Failed => "failed",
        AgentSessionState::Cancelled => "cancelled",
        AgentSessionState::Archived => "archived",
    }
}

const fn run_state_name(state: AgentRunState) -> &'static str {
    match state {
        AgentRunState::Planning => "planning",
        AgentRunState::Executing => "executing",
        AgentRunState::AwaitingApproval => "awaiting_approval",
        AgentRunState::Paused => "paused",
        AgentRunState::NeedsInput => "needs_input",
        AgentRunState::Evaluating => "evaluating",
        AgentRunState::Completed => "completed",
        AgentRunState::Failed => "failed",
        AgentRunState::Cancelled => "cancelled",
    }
}

#[cfg(test)]
mod tests {
    use super::{
        CliOptions, create_session, create_workspace, demonstrate_m2_services,
        demonstrate_m3_recovery, ensure_state_database_with, negotiate, parse_args, run_m4_demo,
        start_run, stream_run,
    };
    use loom_core::{AgentSessionState, RunId};
    use loom_model::ModelId;
    use loom_server::InProcessBackend;
    use std::net::SocketAddr;

    fn arguments<'a>(values: &'a [&str]) -> impl Iterator<Item = String> + 'a {
        values.iter().map(|value| (*value).to_owned())
    }

    fn parse_error(values: &[&str]) -> super::LoomError {
        parse_args(arguments(values))
            .err()
            .expect("arguments should be rejected")
    }

    #[test]
    fn parser_keeps_defaults_and_accepts_separate_and_equals_values() {
        let defaults = parse_args(arguments(&[])).unwrap().unwrap();
        assert_eq!(defaults.name, "M1 demo");
        assert_eq!(defaults.model.as_str(), "deterministic/demo");
        assert_eq!(defaults.bind.to_string(), "127.0.0.1:8765");
        assert!(!defaults.serve);

        let options = parse_args(arguments(&[
            "--name",
            "separate",
            "--task=inline task",
            "--model",
            "model-z",
            "--root=/tmp/work",
            "--persistence",
            "/tmp/loom.db",
            "--bind=0.0.0.0:9",
            "--token",
            "secret",
            "--login=github-copilot",
            "--manual-approval",
            "--m2-demo",
            "--m3-demo",
            "--m4-demo",
            "--serve",
        ]))
        .unwrap()
        .unwrap();
        assert_eq!(options.name, "separate");
        assert_eq!(options.task, "inline task");
        assert_eq!(options.model.as_str(), "model-z");
        assert_eq!(options.root.unwrap().to_str(), Some("/tmp/work"));
        assert_eq!(options.persistence.unwrap().to_str(), Some("/tmp/loom.db"));
        assert_eq!(options.bind.to_string(), "0.0.0.0:9");
        assert_eq!(options.token.as_deref(), Some("secret"));
        assert_eq!(options.login_provider.as_deref(), Some("github-copilot"));
        assert!(options.manual_approval && options.m2_demo && options.m3_demo);
        assert!(options.m4_demo && options.serve);
    }

    #[test]
    fn parser_rejects_missing_values_invalid_bind_and_unknown_arguments() {
        for flag in [
            "--name",
            "--task",
            "--model",
            "--root",
            "--bind",
            "--token",
            "--login",
            "--persistence",
        ] {
            let error = parse_error(&[flag]);
            assert!(
                error.message.contains("requires a value"),
                "{flag}: {error}"
            );
        }
        for bind in ["bad", "--bind=bad"] {
            let error = parse_error(&["--bind", bind]);
            assert!(error.message.contains("socket address"));
        }
        assert!(
            parse_error(&["--mystery"])
                .message
                .contains("unknown argument")
        );
    }

    #[test]
    fn help_short_circuits_argument_parsing() {
        assert!(
            parse_args(arguments(&["--help", "--invalid"]))
                .unwrap()
                .is_none()
        );
        assert!(parse_args(arguments(&["-h"])).unwrap().is_none());
    }

    #[test]
    fn native_cli_workflow_uses_the_protocol_for_session_run_and_services() {
        let connection = InProcessBackend::new().connect();
        negotiate(&connection).unwrap();

        let workspace = create_workspace(&connection, "CLI test workspace").unwrap();
        let session = create_session(&connection, workspace.id, "CLI test session").unwrap();
        assert_eq!(session.state, AgentSessionState::Idle);

        let options = CliOptions {
            name: "CLI test session".to_owned(),
            task: "Say hello in one sentence.".to_owned(),
            model: ModelId::new("deterministic/demo"),
            root: None,
            manual_approval: true,
            m2_demo: false,
            m3_demo: false,
            persistence: None,
            serve: false,
            m4_demo: false,
            bind: "127.0.0.1:0".parse::<SocketAddr>().unwrap(),
            token: None,
            login_provider: None,
            reset_state: false,
            allow_insecure_remote: false,
        };
        let run_id = start_run(&connection, session.id, &options).unwrap();
        stream_run(&connection, session.id, run_id, false).unwrap();

        demonstrate_m2_services(&connection, session.id).unwrap();
    }

    #[test]
    fn m4_demo_reconnects_a_remote_client_and_resumes_approval() {
        let root =
            std::env::temp_dir().join(format!("loom-cli-m4-{}", loom_core::WorkspaceId::new()));
        std::fs::create_dir_all(&root).unwrap();
        let options = CliOptions {
            name: "M4 test".to_owned(),
            task: "Say hello in one sentence.".to_owned(),
            model: ModelId::new("deterministic/demo"),
            root: Some(root.clone()),
            manual_approval: false,
            m2_demo: false,
            m3_demo: false,
            persistence: None,
            serve: false,
            m4_demo: true,
            bind: "127.0.0.1:0".parse::<SocketAddr>().unwrap(),
            token: None,
            login_provider: None,
            reset_state: false,
            allow_insecure_remote: false,
        };

        let result = run_m4_demo(options);
        std::fs::remove_dir_all(root).unwrap();
        result.unwrap();
    }

    #[test]
    fn m3_demo_recovers_session_run_and_event_history_from_persistence() {
        let path = std::env::temp_dir().join(format!("loom-cli-m3-{}.db", loom_core::RunId::new()));
        let backend = InProcessBackend::new_persistent(&path).unwrap();
        let connection = backend.connect();
        negotiate(&connection).unwrap();
        let workspace = create_workspace(&connection, "M3 test workspace").unwrap();
        let session = create_session(&connection, workspace.id, "M3 test session").unwrap();
        let options = CliOptions {
            name: "M3 test session".to_owned(),
            task: "Say hello in one sentence.".to_owned(),
            model: ModelId::new("deterministic/demo"),
            root: None,
            manual_approval: false,
            m2_demo: false,
            m3_demo: true,
            persistence: Some(path.clone()),
            serve: false,
            m4_demo: false,
            bind: "127.0.0.1:0".parse::<SocketAddr>().unwrap(),
            token: None,
            login_provider: None,
            reset_state: false,
            allow_insecure_remote: false,
        };
        let run_id = start_run(&connection, session.id, &options).unwrap();
        stream_run(&connection, session.id, run_id, false).unwrap();

        demonstrate_m3_recovery(backend, connection, &path, true, session.id, run_id).unwrap();
        assert!(!path.exists());
    }

    #[test]
    fn cli_state_reset_is_opt_in_and_wipes_only_when_requested() {
        let path = std::env::temp_dir().join(format!("loom-cli-state-{}.db", RunId::new()));
        // A file that looks like SQLite but declares an older schema version.
        let mut header = [0u8; 100];
        header[..16].copy_from_slice(b"SQLite format 3\0");
        header[60..64].copy_from_slice(&40u32.to_be_bytes());
        std::fs::write(&path, header).unwrap();

        // Declining the interactive prompt preserves the incompatible database.
        assert!(ensure_state_database_with(&path, false, |_, _| Ok(false)).is_err());
        assert!(path.exists());

        // Confirming the prompt wipes it so a fresh baseline can be created.
        ensure_state_database_with(&path, false, |_, _| Ok(true)).unwrap();
        assert!(!path.exists());

        // An explicit reset wipes it without ever prompting.
        std::fs::write(&path, header).unwrap();
        ensure_state_database_with(&path, true, |_, _| panic!("--reset-state must not prompt"))
            .unwrap();
        assert!(!path.exists());
        std::fs::remove_file(&path).ok();
    }

    #[cfg(test)]
    mod cli_args_tests {
        use super::*;

        fn parse(args: &[&str]) -> Result<Option<CliOptions>, loom_core::LoomError> {
            parse_args(args.iter().map(|value| (*value).to_owned()))
        }

        #[test]
        fn parse_args_supports_spaced_forms() {
            let options = parse(&[
                "--name",
                "n",
                "--task",
                "t",
                "--model",
                "m",
                "--root",
                "/r",
                "--manual-approval",
                "--m2-demo",
                "--m3-demo",
                "--serve",
                "--m4-demo",
                "--reset-state",
                "--allow-insecure-remote",
                "--bind",
                "127.0.0.1:9000",
                "--token",
                "tok",
                "--login",
                "github-copilot",
                "--persistence",
                "/p",
            ])
            .unwrap()
            .unwrap();
            assert_eq!(options.name, "n");
            assert_eq!(options.task, "t");
            assert_eq!(options.model, ModelId::new("m"));
            assert_eq!(options.root.as_deref(), Some(std::path::Path::new("/r")));
            assert!(options.manual_approval);
            assert!(options.m2_demo);
            assert!(options.m3_demo);
            assert!(options.serve);
            assert!(options.m4_demo);
            assert!(options.reset_state);
            assert!(options.allow_insecure_remote);
            assert_eq!(options.bind.to_string(), "127.0.0.1:9000");
            assert_eq!(options.token.as_deref(), Some("tok"));
            assert_eq!(options.login_provider.as_deref(), Some("github-copilot"));
            assert_eq!(
                options.persistence.as_deref(),
                Some(std::path::Path::new("/p"))
            );
        }

        #[test]
        fn parse_args_supports_equals_forms() {
            let options = parse(&[
                "--name=n2",
                "--task=t2",
                "--model=m2",
                "--root=/r2",
                "--persistence=/p2",
                "--bind=127.0.0.1:9001",
                "--token=tok2",
                "--login=github-copilot",
            ])
            .unwrap()
            .unwrap();
            assert_eq!(options.name, "n2");
            assert_eq!(options.task, "t2");
            assert_eq!(options.model, ModelId::new("m2"));
            assert_eq!(options.root.as_deref(), Some(std::path::Path::new("/r2")));
            assert_eq!(
                options.persistence.as_deref(),
                Some(std::path::Path::new("/p2"))
            );
            assert_eq!(options.bind.to_string(), "127.0.0.1:9001");
            assert_eq!(options.token.as_deref(), Some("tok2"));
            assert_eq!(options.login_provider.as_deref(), Some("github-copilot"));
        }

        #[test]
        fn parse_args_defaults_and_help() {
            let options = parse(&[]).unwrap().unwrap();
            assert_eq!(options.name, "M1 demo");
            assert_eq!(options.model, ModelId::new("deterministic/demo"));
            assert!(!options.serve);
            assert!(!options.allow_insecure_remote);
            assert!(parse(&["--help"]).unwrap().is_none());
            assert!(parse(&["-h"]).unwrap().is_none());
        }

        #[test]
        fn parse_args_rejects_bad_input() {
            assert!(parse(&["--unknown"]).is_err());
            assert!(parse(&["--name"]).is_err());
            assert!(parse(&["--bind", "not-an-address"]).is_err());
            assert!(parse(&["--bind=bad"]).is_err());
        }
    }
}
