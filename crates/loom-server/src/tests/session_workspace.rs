//! In-process tests: session workspace.

#[allow(unused_imports)]
use super::support::*;
use super::*;

#[test]
fn workspace_config_is_persisted_and_excludes_access_tokens() {
    let path =
        std::env::temp_dir().join(format!("loom-workspace-config-{}.db", WorkspaceId::new()));
    let workspace_id;
    let config = WorkspaceConfig {
        revision: 1,
        cpu_pulse_threshold_percent: 37,
        project_agent_concurrency: 2,
        worker_nodes: vec![WorkerNodeConfig {
            url: "wss://worker.example/ws".to_owned(),
        }],
    };
    {
        let backend = InProcessBackend::new_persistent(&path).unwrap();
        let connection = backend.connect();
        negotiate_m5(&connection);
        let created = connection.request(RequestEnvelope::new(ClientRequest::Workspace(
            WorkspaceRequest::CreateWorkspace {
                name: "Config test".to_owned(),
            },
        )));
        let Ok(ServerResponse::Workspace(WorkspaceResponse::WorkspaceCreated(workspace))) =
            created.result
        else {
            panic!("expected workspace creation");
        };
        workspace_id = workspace.id;
        let response = connection.request(RequestEnvelope::new(ClientRequest::Workspace(
            WorkspaceRequest::SetWorkspaceConfigForWorkspace {
                workspace_id: workspace.id,
                config: config.clone(),
            },
        )));
        assert!(matches!(
            response.result,
            Ok(ServerResponse::Workspace(
                WorkspaceResponse::WorkspaceConfigUpdated
            ))
        ));
        backend.shutdown().unwrap();
    }

    {
        let backend = InProcessBackend::new_persistent(&path).unwrap();
        let connection = backend.connect();
        negotiate_m5(&connection);
        let response = connection.request(RequestEnvelope::new(ClientRequest::Workspace(
            WorkspaceRequest::GetWorkspaceConfigForWorkspace { workspace_id },
        )));
        assert!(matches!(
            response.result,
            Ok(ServerResponse::Workspace(WorkspaceResponse::WorkspaceConfig(saved))) if saved == config
        ));
        let response = connection.request(RequestEnvelope::new(ClientRequest::Workspace(
            WorkspaceRequest::SetWorkspaceConfigForWorkspace {
                workspace_id,
                config: WorkspaceConfig {
                    revision: 0,
                    cpu_pulse_threshold_percent: 5,
                    project_agent_concurrency: 4,
                    worker_nodes: vec![WorkerNodeConfig {
                        url: "wss://stale.example/ws".to_owned(),
                    }],
                },
            },
        )));
        assert!(matches!(
            response.result,
            Ok(ServerResponse::Workspace(
                WorkspaceResponse::WorkspaceConfigUpdated
            ))
        ));
        let response = connection.request(RequestEnvelope::new(ClientRequest::Workspace(
            WorkspaceRequest::GetWorkspaceConfigForWorkspace { workspace_id },
        )));
        assert!(matches!(
            response.result,
            Ok(ServerResponse::Workspace(WorkspaceResponse::WorkspaceConfig(saved))) if saved == config
        ));
        for url in [
            "wss://worker.example/ws?%61ccess_token=secret",
            "wss://user:secret@worker.example/ws",
        ] {
            let response = connection.request(RequestEnvelope::new(ClientRequest::Workspace(
                WorkspaceRequest::SetWorkspaceConfigForWorkspace {
                    workspace_id,
                    config: WorkspaceConfig {
                        revision: 2,
                        cpu_pulse_threshold_percent: 5,
                        project_agent_concurrency: 4,
                        worker_nodes: vec![WorkerNodeConfig {
                            url: url.to_owned(),
                        }],
                    },
                },
            )));
            assert!(response.result.is_err());
        }
        let invalid_concurrency = connection.request(RequestEnvelope::new(
            ClientRequest::Workspace(WorkspaceRequest::SetWorkspaceConfigForWorkspace {
                workspace_id,
                config: WorkspaceConfig {
                    revision: 2,
                    project_agent_concurrency: 0,
                    ..WorkspaceConfig::default()
                },
            }),
        ));
        assert!(matches!(
            invalid_concurrency.result,
            Err(error) if error.code == ErrorCode::InvalidRequest
        ));
        backend.shutdown().unwrap();
    }
    std::fs::remove_file(path).unwrap();
}

#[test]
fn exposes_workspace_terminal_task_and_checkpoint_controls() {
    let root = git_repository();
    let backend = InProcessBackend::new();
    let connection = backend.connect();
    negotiate_m2(&connection);
    let workspace = connection.request(RequestEnvelope::new(ClientRequest::Workspace(
        WorkspaceRequest::CreateWorkspace {
            name: "Filesystem controls".to_owned(),
        },
    )));
    let ServerResponse::Workspace(WorkspaceResponse::WorkspaceCreated(workspace)) =
        workspace.result.unwrap()
    else {
        panic!("unexpected workspace response");
    };
    let created = connection.request(RequestEnvelope::new(ClientRequest::Workspace(
        WorkspaceRequest::CreateAgentSessionInWorkspace {
            workspace_id: workspace.id,
            name: "Filesystem controls".to_owned(),
        },
    )));
    let ServerResponse::Session(SessionResponse::AgentSessionCreated(session)) =
        created.result.unwrap()
    else {
        panic!("unexpected session response");
    };
    let session_id = session.id;
    let attached = connection.request(RequestEnvelope::new(ClientRequest::Repository(
        RepositoryRequest::AttachSessionRepository {
            session_id,
            source: root.display().to_string(),
            path: "repo".to_owned(),
            revision: None,
        },
    )));
    assert!(matches!(
        attached.result,
        Ok(ServerResponse::Repository(
            RepositoryResponse::SessionRepositoryAttached(_)
        ))
    ));
    let snapshot = connection.request(RequestEnvelope::new(ClientRequest::Filesystem(
        FilesystemRequest::GetSessionFilesystemSnapshot { session_id },
    )));
    let ServerResponse::Filesystem(FilesystemResponse::SessionFilesystemSnapshot(snapshot)) =
        snapshot.result.unwrap()
    else {
        panic!("unexpected filesystem snapshot");
    };
    assert!(
        snapshot
            .entries
            .iter()
            .any(|entry| entry.path == "repo/README.md")
    );
    let file = connection.request(RequestEnvelope::new(ClientRequest::Filesystem(
        FilesystemRequest::ReadSessionFile {
            session_id,
            path: "repo/README.md".to_owned(),
        },
    )));
    let revision = match file.result.unwrap() {
        ServerResponse::Filesystem(FilesystemResponse::SessionFilesystemFile(file)) => {
            file.revision
        }
        response => panic!("unexpected response: {response:?}"),
    };
    let checkpoint = connection.request(RequestEnvelope::new(ClientRequest::Filesystem(
        FilesystemRequest::CreateSessionCheckpoint {
            session_id,
            label: "before user edit".to_owned(),
        },
    )));
    let checkpoint_id = match checkpoint.result.unwrap() {
        ServerResponse::Filesystem(FilesystemResponse::CheckpointCreated(checkpoint)) => {
            checkpoint.id
        }
        response => panic!("unexpected response: {response:?}"),
    };
    let edit = connection.request(RequestEnvelope::new(ClientRequest::Filesystem(
        FilesystemRequest::ApplySessionFilesystemEdit {
            session_id,
            edit: WorkspaceEdit {
                path: "repo/README.md".to_owned(),
                old_text: "source".to_owned(),
                new_text: "user".to_owned(),
                expected_revision: Some(revision),
            },
        },
    )));
    assert!(matches!(
        edit.result,
        Ok(ServerResponse::Filesystem(
            FilesystemResponse::WorkspaceEditApplied(_)
        ))
    ));
    let changes = connection.request(RequestEnvelope::new(ClientRequest::Filesystem(
        FilesystemRequest::GetSessionFilesystemChanges {
            session_id,
            after_sequence: None,
        },
    )));
    let ServerResponse::Filesystem(FilesystemResponse::SessionFilesystemChanges {
        changes, ..
    }) = changes.result.unwrap()
    else {
        panic!("unexpected filesystem changes response");
    };
    assert!(changes.iter().any(|event| event.path == "repo/README.md"));
    let revert = connection.request(RequestEnvelope::new(ClientRequest::Filesystem(
        FilesystemRequest::RevertSessionCheckpoint {
            session_id,
            checkpoint_id,
        },
    )));
    assert_eq!(
        revert.result.unwrap_err().code,
        ErrorCode::Conflict,
        "checkpoint revert must preserve the intervening user edit"
    );
    let file = connection.request(RequestEnvelope::new(ClientRequest::Filesystem(
        FilesystemRequest::ReadSessionFile {
            session_id,
            path: "repo/README.md".to_owned(),
        },
    )));
    assert!(matches!(
        file.result,
        Ok(ServerResponse::Filesystem(FilesystemResponse::SessionFilesystemFile(file))) if file.content == "user\n"
    ));

    let terminal_command = if cfg!(windows) {
        (
            "cmd".to_owned(),
            vec!["/C".to_owned(), "echo terminal".to_owned()],
        )
    } else {
        ("printf".to_owned(), vec!["terminal".to_owned()])
    };
    let terminal = connection.request(RequestEnvelope::new(ClientRequest::Terminal(
        TerminalRequest::OpenSessionTerminal {
            session_id,
            command: terminal_command.0,
            args: terminal_command.1,
            cwd: None,
        },
    )));
    let terminal_id = match terminal.result.unwrap() {
        ServerResponse::Terminal(TerminalResponse::TerminalOpened(snapshot)) => snapshot.id,
        response => panic!("unexpected response: {response:?}"),
    };
    let mut terminal_done = false;
    for _ in 0..100 {
        let events = connection.request(RequestEnvelope::new(ClientRequest::Terminal(
            TerminalRequest::GetSessionTerminalEvents {
                session_id,
                terminal_id,
                after_sequence: None,
            },
        )));
        let ServerResponse::Terminal(TerminalResponse::TerminalEvents { events }) =
            events.result.unwrap()
        else {
            panic!("unexpected terminal event response");
        };
        if events.iter().any(|event| {
            matches!(
                event.event,
                TerminalEvent::Exited {
                    status: loom_process::TerminalStatus::Exited,
                    ..
                }
            )
        }) {
            terminal_done = true;
            break;
        }
        thread::sleep(Duration::from_millis(2));
    }
    assert!(terminal_done);

    let task_command = if cfg!(windows) {
        (
            "cmd".to_owned(),
            vec!["/C".to_owned(), "echo artifact>artifact.txt".to_owned()],
        )
    } else {
        (
            "sh".to_owned(),
            vec!["-c".to_owned(), "printf artifact > artifact.txt".to_owned()],
        )
    };
    let task = connection.request(RequestEnvelope::new(ClientRequest::Task(
        TaskRequest::StartSessionTask {
            session_id,
            spec: TaskSpec {
                kind: TaskKind::Test,
                label: "M2 task".to_owned(),
                command: task_command.0,
                args: task_command.1,
                cwd: Some("repo".to_owned()),
                output_limit_bytes: Some(4096),
                artifact_paths: vec!["repo/artifact.txt".to_owned()],
            },
        },
    )));
    let task_id = match task.result.unwrap() {
        ServerResponse::Task(TaskResponse::TaskStarted(snapshot)) => snapshot.id,
        response => panic!("unexpected response: {response:?}"),
    };
    let mut task_done = false;
    for _ in 0..100 {
        let current = connection.request(RequestEnvelope::new(ClientRequest::Task(
            TaskRequest::GetSessionTask {
                session_id,
                task_id,
            },
        )));
        let ServerResponse::Task(TaskResponse::Task(snapshot)) = current.result.unwrap() else {
            panic!("unexpected task response");
        };
        if matches!(
            snapshot.status,
            TaskStatus::Completed | TaskStatus::Failed | TaskStatus::Cancelled
        ) {
            assert!(snapshot.artifacts[0].exists);
            task_done = true;
            break;
        }
        thread::sleep(Duration::from_millis(2));
    }
    assert!(task_done);
    let listed = connection.request(RequestEnvelope::new(ClientRequest::Task(
        TaskRequest::ListSessionTasks { session_id },
    )));
    let ServerResponse::Task(TaskResponse::Tasks { tasks }) = listed.result.unwrap() else {
        panic!("unexpected task list response");
    };
    assert!(tasks.iter().any(|task| task.id == task_id));
    let task_events = connection.request(RequestEnvelope::new(ClientRequest::Task(
        TaskRequest::GetSessionTaskEvents {
            session_id,
            task_id,
            after_sequence: None,
        },
    )));
    let ServerResponse::Task(TaskResponse::TaskEvents { events }) = task_events.result.unwrap()
    else {
        panic!("unexpected task event response");
    };
    assert!(
        events
            .iter()
            .any(|event| matches!(event.event, TaskEvent::Completed { .. }))
    );

    let control = connection.request(RequestEnvelope::new(ClientRequest::Filesystem(
        FilesystemRequest::TakeSessionFilesystemControl {
            session_id,
            control: WorkspaceControl::User,
        },
    )));
    assert!(matches!(
        control.result,
        Ok(ServerResponse::Filesystem(
            FilesystemResponse::WorkspaceControl(WorkspaceControl::User)
        ))
    ));
    fs::remove_dir_all(&backend.session_root_base).unwrap();
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn persistent_backend_recovers_transcript_workspace_and_pending_approval() {
    let persistence =
        std::env::temp_dir().join(format!("loom-server-state-{}.db", WorkspaceId::new()));
    let session_root_base;
    let (session_id, run_id, approval) = {
        let backend = InProcessBackend::new_persistent(&persistence).unwrap();
        session_root_base = backend.session_root_base.clone();
        let connection = backend.connect();
        negotiate_m3(&connection);
        let created = connection.request(RequestEnvelope::new(ClientRequest::Workspace(
            WorkspaceRequest::CreateWorkspace {
                name: "Durable workspace".to_owned(),
            },
        )));
        let ServerResponse::Workspace(WorkspaceResponse::WorkspaceCreated(workspace)) =
            created.result.unwrap()
        else {
            panic!("unexpected workspace response");
        };
        let session = connection.request(RequestEnvelope::new(ClientRequest::Workspace(
            WorkspaceRequest::CreateAgentSessionInWorkspace {
                workspace_id: workspace.id,
                name: "durable run".to_owned(),
            },
        )));
        let session_id = match session.result.unwrap() {
            ServerResponse::Session(SessionResponse::AgentSessionCreated(snapshot)) => snapshot.id,
            response => panic!("unexpected response: {response:?}"),
        };
        connection.request(RequestEnvelope::new(ClientRequest::Session(
            SessionRequest::SetSessionApprovalPolicy {
                session_id,
                policy: ApprovalPolicy::default(),
                auto_approve_actions: Some(false),
            },
        )));
        let started = connection.request(RequestEnvelope::new(ClientRequest::Run(
            RunRequest::StartSessionAgentRunWithOptions {
                session_id,
                task: "create a demo file".to_owned(),
                model: ModelId::new("deterministic/demo"),
                system_instructions: Some("Be concise.".to_owned()),
                repository_instructions: Some("Keep changes focused.".to_owned()),
                limits: loom_core::SessionLimits {
                    max_tool_calls: Some(20),
                    ..Default::default()
                },
                context: ContextAssemblyOptions {
                    context_window: Some(8_192),
                    max_input_tokens: Some(4_096),
                    reserved_output_tokens: Some(1_024),
                },
            },
        )));
        let run_id = match started.result.unwrap() {
            ServerResponse::Run(RunResponse::AgentRunStarted(snapshot)) => snapshot.id,
            response => panic!("unexpected response: {response:?}"),
        };
        await_settled_run(&connection, run_id);
        backend.flush().unwrap();
        let persisted = FilePersistence::open(&persistence).unwrap();
        let runtime_config = persisted.load_run_runtime_config(run_id).unwrap().unwrap();
        let system_instructions = runtime_config.system_instructions.unwrap();
        assert!(system_instructions.starts_with("Be concise."));
        assert!(system_instructions.contains("project manager for this project"));
        assert_eq!(
            runtime_config.repository_instructions.as_deref(),
            Some("Keep changes focused.")
        );
        assert_eq!(runtime_config.context_options.context_window, Some(8_192));
        assert_eq!(runtime_config.limits.max_tool_calls, Some(20));
        let events = match connection
            .request(RequestEnvelope::new(ClientRequest::Events(
                EventsRequest::GetSessionEvents {
                    session_id: Some(session_id),
                    workspace_id: None,
                    after_sequence: None,
                    stream_epoch: None,
                },
            )))
            .result
            .unwrap()
        {
            ServerResponse::Events(EventsResponse::SessionEvents { events, .. }) => events,
            response => panic!("unexpected response: {response:?}"),
        };
        let approval = events
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
                } => Some((call.id, *attempt_id, *control_revision)),
                _ => None,
            })
            .unwrap();
        backend.shutdown().unwrap();
        (session_id, run_id, approval)
    };
    assert!(persistence.is_file());

    let backend = InProcessBackend::new_persistent(&persistence).unwrap();
    assert!(backend.journal().unwrap().events.is_empty());
    assert!(backend.runs().unwrap().is_empty());
    assert!(backend.persisted_runs().unwrap().contains_key(&run_id));
    let connection = backend.connect();
    negotiate_m3(&connection);
    let recovered_session = connection.request(RequestEnvelope::new(ClientRequest::Session(
        SessionRequest::GetAgentSessionSnapshot { session_id },
    )));
    let ServerResponse::Session(SessionResponse::AgentSessionSnapshot(recovered_session)) =
        recovered_session.result.unwrap()
    else {
        panic!("unexpected recovered session snapshot response");
    };
    assert!(!recovered_session.auto_approve_actions);
    assert_eq!(recovered_session.approval_policy, ApprovalPolicy::default());
    let detail = connection.request(RequestEnvelope::new(ClientRequest::Run(
        RunRequest::GetAgentRunSnapshot { run_id },
    )));
    let ServerResponse::Run(RunResponse::AgentRunSnapshot(detail)) = detail.result.unwrap() else {
        panic!("unexpected run snapshot response");
    };
    assert_eq!(detail.run.id, run_id);
    assert!(detail.messages.iter().any(|message| {
        message.role == loom_model::MessageRole::User
            && message.content.contains("create a demo file")
    }));
    assert!(detail.messages.iter().any(|message| {
        message.role == loom_model::MessageRole::Assistant && !message.tool_calls.is_empty()
    }));
    assert!(
        detail
            .activities
            .iter()
            .any(|activity| { activity.status == AgentActivityStatus::AwaitingApproval })
    );
    assert!(backend.runs().unwrap().is_empty());
    let recovered = connection.request(RequestEnvelope::new(ClientRequest::Run(
        RunRequest::GetAgentRun { run_id },
    )));
    let ServerResponse::Run(RunResponse::AgentRun(snapshot)) = recovered.result.unwrap() else {
        panic!("unexpected run response");
    };
    assert_eq!(snapshot.state, AgentRunState::AwaitingApproval);
    assert_eq!(snapshot.attempt_id, approval.1);
    assert_eq!(snapshot.control_revision, approval.2);
    let reconstructed_state = connection.run_handle(run_id).unwrap().state();
    let system_instructions = reconstructed_state.task.system_instructions.unwrap();
    assert!(system_instructions.starts_with("Be concise."));
    assert!(system_instructions.contains("project manager for this project"));
    assert_eq!(
        reconstructed_state.options.context.context_window,
        Some(8_192)
    );
    assert_eq!(reconstructed_state.options.limits.max_tool_calls, Some(20));
    let recovered_interactions = backend
        .persistence
        .as_ref()
        .unwrap()
        .load_run_interactions(run_id)
        .unwrap();
    assert!(recovered_interactions.iter().any(|interaction| {
        interaction.attempt_id == approval.1
            && interaction.control_revision == approval.2
            && interaction.tool_call_id == Some(approval.0)
            && interaction.status == AgentInteractionStatus::Pending
    }));
    let recovered_snapshot = connection.request(RequestEnvelope::new(ClientRequest::Run(
        RunRequest::GetAgentRunSnapshot { run_id },
    )));
    let ServerResponse::Run(RunResponse::AgentRunSnapshot(projection)) =
        recovered_snapshot.result.unwrap()
    else {
        panic!("unexpected run snapshot response");
    };
    assert!(!projection.activities.is_empty());
    assert!(
        projection
            .activities
            .iter()
            .any(|activity| activity.status == AgentActivityStatus::AwaitingApproval)
    );
    let page = connection.request(RequestEnvelope::new(ClientRequest::Run(
        RunRequest::GetAgentRunMessagePage {
            run_id,
            before_ordinal: None,
            limit: MAX_AGENT_RUN_MESSAGE_PAGE_SIZE,
        },
    )));
    let ServerResponse::Run(RunResponse::AgentRunMessagePage {
        run_id: page_run_id,
        messages,
    }) = page.result.unwrap()
    else {
        panic!("unexpected run message page response");
    };
    assert_eq!(page_run_id, run_id);
    assert!(!messages.is_empty());
    assert!(
        messages
            .windows(2)
            .all(|pair| pair[0].ordinal > pair[1].ordinal),
        "message page must be in descending keyset order"
    );
    let oversized_page = connection.request(RequestEnvelope::new(ClientRequest::Run(
        RunRequest::GetAgentRunMessagePage {
            run_id,
            before_ordinal: None,
            limit: MAX_AGENT_RUN_MESSAGE_PAGE_SIZE + 1,
        },
    )));
    assert_eq!(
        oversized_page.result.unwrap_err().code,
        ErrorCode::InvalidRequest
    );
    let message_header = messages
        .iter()
        .find(|message| message.content_bytes > 0)
        .unwrap();
    let range = connection.request(RequestEnvelope::new(ClientRequest::Run(
        RunRequest::GetAgentRunMessageContentRange {
            run_id,
            message_ordinal: message_header.ordinal,
            byte_offset: 0,
            length: message_header.content_bytes.min(32) as u32,
        },
    )));
    let ServerResponse::Run(RunResponse::AgentRunMessageContentRange {
        run_id: range_run_id,
        message_ordinal,
        byte_offset,
        content,
    }) = range.result.unwrap()
    else {
        panic!("unexpected run message content response");
    };
    assert_eq!(range_run_id, run_id);
    assert_eq!(message_ordinal, message_header.ordinal);
    assert_eq!(byte_offset, 0);
    assert!(!content.is_empty());
    let expected_content = projection.messages[message_ordinal as usize]
        .content
        .as_bytes();
    assert_eq!(content, expected_content[..content.len()]);
    let oversized_range = connection.request(RequestEnvelope::new(ClientRequest::Run(
        RunRequest::GetAgentRunMessageContentRange {
            run_id,
            message_ordinal,
            byte_offset: 0,
            length: MAX_AGENT_RUN_MESSAGE_CONTENT_RANGE_BYTES + 1,
        },
    )));
    assert_eq!(
        oversized_range.result.unwrap_err().code,
        ErrorCode::InvalidRequest
    );
    let legacy_connection = backend.connect();
    let legacy_negotiation = legacy_connection.request(RequestEnvelope::new(
        ClientRequest::Control(ControlRequest::Negotiate {
            client_version: ProtocolVersion::new(2, 0),
            capabilities: backend.supported_capabilities.clone(),
        }),
    ));
    assert_eq!(
        legacy_negotiation.result.unwrap_err().code,
        ErrorCode::UnsupportedProtocol
    );
    let protocol_3_connection = backend.connect();
    let protocol_3_negotiation = protocol_3_connection.request(RequestEnvelope::new(
        ClientRequest::Control(ControlRequest::Negotiate {
            client_version: ProtocolVersion::new(3, 0),
            capabilities: backend.supported_capabilities.clone(),
        }),
    ));
    assert_eq!(
        protocol_3_negotiation.result.unwrap_err().code,
        ErrorCode::UnsupportedProtocol
    );
    let protocol_4_connection = backend.connect();
    let protocol_4_negotiation = protocol_4_connection.request(RequestEnvelope::with_version(
        ProtocolVersion::new(4, 1),
        ClientRequest::Control(ControlRequest::Negotiate {
            client_version: ProtocolVersion::new(4, 1),
            capabilities: backend.supported_capabilities.clone(),
        }),
    ));
    assert_eq!(
        protocol_4_negotiation.result.unwrap_err().code,
        ErrorCode::UnsupportedProtocol
    );
    let protocol_5_connection = backend.connect();
    let protocol_5_negotiation = protocol_5_connection.request(RequestEnvelope::with_version(
        ProtocolVersion::new(5, 0),
        ClientRequest::Control(ControlRequest::Negotiate {
            client_version: ProtocolVersion::new(5, 0),
            capabilities: backend.supported_capabilities.clone(),
        }),
    ));
    assert_eq!(
        protocol_5_negotiation.result.unwrap_err().code,
        ErrorCode::UnsupportedProtocol
    );
    let protocol_6_connection = backend.connect();
    let protocol_6_negotiation = protocol_6_connection.request(RequestEnvelope::with_version(
        ProtocolVersion::new(6, 0),
        ClientRequest::Control(ControlRequest::Negotiate {
            client_version: ProtocolVersion::new(6, 0),
            capabilities: backend.supported_capabilities.clone(),
        }),
    ));
    assert_eq!(
        protocol_6_negotiation.result.unwrap_err().code,
        ErrorCode::UnsupportedProtocol
    );
    let protocol_7_connection = backend.connect();
    let protocol_7_negotiation = protocol_7_connection.request(RequestEnvelope::with_version(
        ProtocolVersion::new(7, 0),
        ClientRequest::Control(ControlRequest::Negotiate {
            client_version: ProtocolVersion::new(7, 0),
            capabilities: backend.supported_capabilities.clone(),
        }),
    ));
    assert_eq!(
        protocol_7_negotiation.result.unwrap_err().code,
        ErrorCode::UnsupportedProtocol
    );
    let protocol_8_connection = backend.connect();
    let protocol_8_discovery = protocol_8_connection.request(RequestEnvelope::with_version(
        ProtocolVersion::new(8, 0),
        ClientRequest::Control(ControlRequest::DiscoverCapabilities),
    ));
    assert_eq!(
        protocol_8_discovery.result.unwrap_err().code,
        ErrorCode::UnsupportedProtocol
    );
    let protocol_8_negotiation = protocol_8_connection.request(RequestEnvelope::with_version(
        CURRENT_PROTOCOL_VERSION,
        ClientRequest::Control(ControlRequest::Negotiate {
            client_version: ProtocolVersion::new(8, 0),
            capabilities: backend.supported_capabilities.clone(),
        }),
    ));
    assert_eq!(
        protocol_8_negotiation.result.unwrap_err().code,
        ErrorCode::UnsupportedProtocol
    );
    let protocol_9_connection = backend.connect();
    let protocol_9_discovery = protocol_9_connection.request(RequestEnvelope::with_version(
        ProtocolVersion::new(9, 0),
        ClientRequest::Control(ControlRequest::DiscoverCapabilities),
    ));
    assert_eq!(
        protocol_9_discovery.result.unwrap_err().code,
        ErrorCode::UnsupportedProtocol
    );
    let protocol_9_negotiation = protocol_9_connection.request(RequestEnvelope::with_version(
        CURRENT_PROTOCOL_VERSION,
        ClientRequest::Control(ControlRequest::Negotiate {
            client_version: ProtocolVersion::new(9, 0),
            capabilities: backend.supported_capabilities.clone(),
        }),
    ));
    assert_eq!(
        protocol_9_negotiation.result.unwrap_err().code,
        ErrorCode::UnsupportedProtocol
    );

    let capability_limited_connection = backend.connect();
    let capability_limited = CapabilitySet::new(
        backend
            .supported_capabilities
            .iter()
            .copied()
            .filter(|capability| *capability != Capability::ReadAgentRunMessages),
    );
    let current_negotiation = capability_limited_connection.request(RequestEnvelope::new(
        ClientRequest::Control(ControlRequest::Negotiate {
            client_version: CURRENT_PROTOCOL_VERSION,
            capabilities: capability_limited,
        }),
    ));
    assert!(matches!(
        current_negotiation.result,
        Ok(ServerResponse::Control(ControlResponse::Negotiated(_)))
    ));
    let unsupported_page = capability_limited_connection.request(RequestEnvelope::new(
        ClientRequest::Run(RunRequest::GetAgentRunMessagePage {
            run_id,
            before_ordinal: None,
            limit: MAX_AGENT_RUN_MESSAGE_PAGE_SIZE,
        }),
    ));
    assert_eq!(
        unsupported_page.result.unwrap_err().code,
        ErrorCode::CapabilityDenied
    );
    let events = connection.request(RequestEnvelope::new(ClientRequest::Events(
        EventsRequest::GetSessionEvents {
            session_id: Some(session_id),
            workspace_id: None,
            after_sequence: None,
            stream_epoch: None,
        },
    )));
    let ServerResponse::Events(EventsResponse::SessionEvents { events, .. }) =
        events.result.unwrap()
    else {
        panic!("unexpected events response");
    };
    assert!(events.len() >= 5);
    let checkpoint = connection.request(RequestEnvelope::new(ClientRequest::Run(
        RunRequest::GetRunCheckpoint { run_id },
    )));
    let ServerResponse::Run(RunResponse::RunCheckpoint(checkpoint)) = checkpoint.result.unwrap()
    else {
        panic!("unexpected checkpoint response");
    };
    assert_eq!(checkpoint.session_id, session_id);

    let wrong_attempt = connection.request(RequestEnvelope::new(ClientRequest::Run(
        RunRequest::ApproveAgentAction {
            run_id,
            attempt_id: loom_core::RunAttemptId::new(),
            expected_control_revision: approval.2,
            tool_call_id: approval.0,
        },
    )));
    assert_eq!(wrong_attempt.result.unwrap_err().code, ErrorCode::Conflict);
    let stale_revision = connection.request(RequestEnvelope::new(ClientRequest::Run(
        RunRequest::ApproveAgentAction {
            run_id,
            attempt_id: approval.1,
            expected_control_revision: approval.2.saturating_sub(1),
            tool_call_id: approval.0,
        },
    )));
    assert_eq!(stale_revision.result.unwrap_err().code, ErrorCode::Conflict);

    let response = connection.request(RequestEnvelope::new(ClientRequest::Run(
        RunRequest::ApproveAgentAction {
            run_id,
            attempt_id: approval.1,
            expected_control_revision: approval.2,
            tool_call_id: approval.0,
        },
    )));
    assert!(response.result.is_ok());
    await_settled_run(&connection, run_id);
    let resolved_interactions = backend
        .persistence
        .as_ref()
        .unwrap()
        .load_run_interactions(run_id)
        .unwrap();
    assert!(resolved_interactions.iter().any(|interaction| {
        interaction.tool_call_id == Some(approval.0)
            && interaction.status == AgentInteractionStatus::Approved
            && interaction.decision == Some(ApprovalDecision::Approved)
    }));
    let command_approval = (0..1_000)
        .find_map(|_| {
            let events = match connection
                .request(RequestEnvelope::new(ClientRequest::Events(
                    EventsRequest::GetSessionEvents {
                        session_id: Some(session_id),
                        workspace_id: None,
                        after_sequence: None,
                        stream_epoch: None,
                    },
                )))
                .result
                .unwrap()
            {
                ServerResponse::Events(EventsResponse::SessionEvents { events, .. }) => events,
                response => panic!("unexpected events response: {response:?}"),
            };
            let approval = events.iter().find_map(|event| match &event.event {
                ServerEvent::Agent {
                    event:
                        AgentEvent::ToolApprovalRequired {
                            call,
                            attempt_id,
                            control_revision,
                            ..
                        },
                } if call.name == "run_command" => Some((call.id, *attempt_id, *control_revision)),
                _ => None,
            });
            approval.or_else(|| {
                thread::sleep(Duration::from_millis(1));
                None
            })
        })
        .expect("run_command approval did not arrive");
    let response = connection.request(RequestEnvelope::new(ClientRequest::Run(
        RunRequest::ApproveAgentAction {
            run_id,
            attempt_id: command_approval.1,
            expected_control_revision: command_approval.2,
            tool_call_id: command_approval.0,
        },
    )));
    assert!(response.result.is_ok());
    let mut usage = match connection
        .request(RequestEnvelope::new(ClientRequest::Usage(
            UsageRequest::GetRunUsage { run_id },
        )))
        .result
        .unwrap()
    {
        ServerResponse::Usage(UsageResponse::RunUsage { usage, .. }) => usage,
        response => panic!("unexpected usage response: {response:?}"),
    };
    for _ in 0..1_000 {
        if usage.input_tokens > 0 {
            break;
        }
        thread::sleep(Duration::from_millis(1));
        usage = match connection
            .request(RequestEnvelope::new(ClientRequest::Usage(
                UsageRequest::GetRunUsage { run_id },
            )))
            .result
            .unwrap()
        {
            ServerResponse::Usage(UsageResponse::RunUsage { usage, .. }) => usage,
            response => panic!("unexpected usage response: {response:?}"),
        };
    }
    assert_eq!(usage.input_tokens, 240);
    assert_eq!(usage.output_tokens, 52);
    assert_eq!(usage.tool_calls, 3);
    let filesystem = connection.request(RequestEnvelope::new(ClientRequest::Filesystem(
        FilesystemRequest::GetSessionFilesystemSnapshot { session_id },
    )));
    assert!(matches!(
        filesystem.result,
        Ok(ServerResponse::Filesystem(
            FilesystemResponse::SessionFilesystemSnapshot(_)
        ))
    ));
    backend.shutdown().unwrap();
    backend.shutdown().unwrap();
    assert_eq!(
        connection
            .request(RequestEnvelope::new(ClientRequest::Usage(
                UsageRequest::GetRunUsage { run_id }
            )))
            .result
            .unwrap_err()
            .code,
        ErrorCode::Conflict
    );
    drop(connection);
    drop(backend);
    let reopened = InProcessBackend::new_persistent(&persistence).unwrap();
    let reopened_connection = reopened.connect();
    negotiate_m3(&reopened_connection);
    let recovered_usage = reopened_connection.request(RequestEnvelope::new(ClientRequest::Usage(
        UsageRequest::GetRunUsage { run_id },
    )));
    let ServerResponse::Usage(UsageResponse::RunUsage { usage, .. }) =
        recovered_usage.result.unwrap()
    else {
        panic!("unexpected recovered usage response");
    };
    assert_eq!(usage.input_tokens, 240);
    assert_eq!(usage.output_tokens, 52);
    let before_retry = reopened_connection.request(RequestEnvelope::new(ClientRequest::Run(
        RunRequest::GetAgentRun { run_id },
    )));
    let ServerResponse::Run(RunResponse::AgentRun(before_retry)) = before_retry.result.unwrap()
    else {
        panic!("unexpected run response before checkpoint retry");
    };
    let prior_attempts = reopened
        .persistence
        .as_ref()
        .unwrap()
        .load_run_attempts(run_id)
        .unwrap();
    assert_eq!(prior_attempts.len(), 1);
    assert_eq!(prior_attempts[0].id, before_retry.attempt_id);
    let retried = reopened_connection.request(RequestEnvelope::new(ClientRequest::Run(
        RunRequest::RetryAgentFromCheckpoint {
            run_id,
            checkpoint_id: checkpoint.id,
        },
    )));
    let ServerResponse::Run(RunResponse::AgentRun(retried)) = retried.result.unwrap() else {
        panic!("unexpected checkpoint retry response");
    };
    assert_ne!(retried.attempt_id, before_retry.attempt_id);
    assert_eq!(
        await_settled_run(&reopened_connection, run_id).state,
        AgentRunState::AwaitingApproval
    );
    let after_retry = reopened_connection.request(RequestEnvelope::new(ClientRequest::Run(
        RunRequest::GetAgentRun { run_id },
    )));
    let ServerResponse::Run(RunResponse::AgentRun(after_retry)) = after_retry.result.unwrap()
    else {
        panic!("unexpected run response after checkpoint retry");
    };
    assert_eq!(after_retry.attempt_id, retried.attempt_id);
    let mut attempts = Vec::new();
    for _ in 0..1_000 {
        attempts = reopened
            .persistence
            .as_ref()
            .unwrap()
            .load_run_attempts(run_id)
            .unwrap();
        if attempts
            .last()
            .is_some_and(|attempt| attempt.state == AgentRunState::AwaitingApproval)
        {
            break;
        }
        thread::sleep(Duration::from_millis(1));
    }
    assert_eq!(attempts.len(), 2);
    assert_eq!(attempts[0].id, before_retry.attempt_id);
    assert_eq!(attempts[0].number, 1);
    assert_eq!(attempts[1].id, retried.attempt_id);
    assert_eq!(attempts[1].number, 2);
    assert_eq!(attempts[1].state, AgentRunState::AwaitingApproval);
    reopened.shutdown().unwrap();
    drop(reopened_connection);
    drop(reopened);

    // Model a crash after execution started but before the runtime could
    // persist its paused recovery state.
    let persistence_store = FilePersistence::open(&persistence).unwrap();
    let mut summary = persistence_store.load_run_summary(run_id).unwrap().unwrap();
    summary.snapshot.state = AgentRunState::Executing;
    summary.snapshot.completed_at = None;
    let mut execution = persistence_store
        .load_run_execution_state(run_id)
        .unwrap()
        .unwrap();
    execution.state = AgentRunState::Executing;
    execution.pending_approval = None;
    execution.pending_input = None;
    summary.execution_state = Some(execution);
    let mut attempts = persistence_store.load_run_attempts(run_id).unwrap();
    let current_attempt = attempts.last_mut().unwrap();
    current_attempt.state = AgentRunState::Executing;
    current_attempt.completed_at = None;
    summary.attempts = Some(attempts);
    let summaries = BTreeMap::from([(run_id, summary)]);
    let sessions = persistence_store.load_sessions().unwrap().unwrap();
    persistence_store
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
    drop(persistence_store);

    let restored = InProcessBackend::new_persistent(&persistence).unwrap();
    assert!(restored.runs().unwrap().is_empty());
    assert_eq!(
        restored
            .persisted_runs()
            .unwrap()
            .get(&run_id)
            .unwrap()
            .snapshot
            .state,
        AgentRunState::Paused
    );
    let recovery_session_id = restored
        .persisted_runs()
        .unwrap()
        .get(&run_id)
        .unwrap()
        .snapshot
        .session_id;
    let restored_connection = restored.connect();
    negotiate_m3(&restored_connection);
    let metadata = restored_connection.request(RequestEnvelope::new(ClientRequest::Session(
        SessionRequest::GetAgentSessionSnapshotMetadata {
            session_id: recovery_session_id,
        },
    )));
    let ServerResponse::Session(SessionResponse::AgentSessionSnapshot(metadata)) =
        metadata.result.unwrap()
    else {
        panic!("unexpected metadata session snapshot response");
    };
    assert!(
        metadata
            .active_run
            .as_ref()
            .is_some_and(|projection| projection.messages.is_empty())
    );
    let initial = restored_connection.request(RequestEnvelope::new(ClientRequest::Session(
        SessionRequest::GetAgentSessionInitialState {
            session_id: recovery_session_id,
        },
    )));
    let ServerResponse::Session(SessionResponse::AgentSessionInitialState(initial)) =
        initial.result.unwrap()
    else {
        panic!("unexpected initial session state response");
    };
    assert_eq!(initial.cursor, initial.projection.latest_sequence);
    assert!(
        initial
            .projection
            .active_run
            .as_ref()
            .is_some_and(|projection| projection.messages.is_empty())
    );
    assert!(restored.runs().unwrap().is_empty());
    let page = restored_connection.request(RequestEnvelope::new(ClientRequest::Run(
        RunRequest::GetAgentRunMessagePage {
            run_id,
            before_ordinal: None,
            limit: 10,
        },
    )));
    let ServerResponse::Run(RunResponse::AgentRunMessagePage { messages, .. }) =
        page.result.unwrap()
    else {
        panic!("unexpected transcript page response");
    };
    assert!(!messages.is_empty());
    let first = messages.first().unwrap();
    assert!(first.content_bytes > 0);
    let content = restored_connection.request(RequestEnvelope::new(ClientRequest::Run(
        RunRequest::GetAgentRunMessageContentRange {
            run_id,
            message_ordinal: first.ordinal,
            byte_offset: 0,
            length: u32::try_from(first.content_bytes.min(128)).unwrap(),
        },
    )));
    let ServerResponse::Run(RunResponse::AgentRunMessageContentRange { content, .. }) =
        content.result.unwrap()
    else {
        panic!("unexpected transcript content response");
    };
    assert!(!content.is_empty());
    let invalid_page = restored_connection.request(RequestEnvelope::new(ClientRequest::Run(
        RunRequest::GetAgentRunTranscriptPage {
            run_id,
            before_ordinal: None,
            limit: 0,
        },
    )));
    assert_eq!(
        invalid_page.result.unwrap_err().code,
        ErrorCode::InvalidRequest
    );
    let transcript_page = restored_connection.request(RequestEnvelope::new(ClientRequest::Run(
        RunRequest::GetAgentRunTranscriptPage {
            run_id,
            before_ordinal: None,
            limit: MAX_AGENT_RUN_TRANSCRIPT_PAGE_SIZE,
        },
    )));
    let ServerResponse::Run(RunResponse::AgentRunTranscriptPage {
        messages,
        next_before,
        has_older,
        ..
    }) = transcript_page.result.unwrap()
    else {
        panic!("unexpected bounded transcript page response");
    };
    assert!(!messages.is_empty());
    assert!(
        messages
            .windows(2)
            .all(|pair| pair[0].ordinal < pair[1].ordinal)
    );
    assert_eq!(next_before, messages.first().map(|message| message.ordinal));
    assert!(!has_older);

    let projection = restored_connection.request(RequestEnvelope::new(ClientRequest::Run(
        RunRequest::GetAgentRunSnapshot { run_id },
    )));
    let ServerResponse::Run(RunResponse::AgentRunSnapshot(projection)) = projection.result.unwrap()
    else {
        panic!("unexpected lazily restored run snapshot response");
    };
    assert_eq!(projection.run.state, AgentRunState::Paused);
    assert!(!projection.messages.is_empty());
    assert!(restored.runs().unwrap().is_empty());
    let execution = restored
        .persistence
        .as_ref()
        .unwrap()
        .load_run_execution_state(run_id)
        .unwrap()
        .unwrap();
    assert_eq!(execution.state, AgentRunState::Paused);
    assert!(execution.pending_tool_execution.is_none());
    let events = restored_connection.request(RequestEnvelope::new(ClientRequest::Events(
        EventsRequest::GetSessionEvents {
            session_id: Some(recovery_session_id),
            workspace_id: None,
            after_sequence: None,
            stream_epoch: None,
        },
    )));
    let ServerResponse::Events(EventsResponse::SessionEvents { events, .. }) =
        events.result.unwrap()
    else {
        panic!("unexpected recovery event response");
    };
    assert!(events.iter().any(|event| matches!(
        &event.event,
        ServerEvent::Agent {
            event: AgentEvent::RunStateChanged {
                run_id: event_run_id,
                state: AgentRunState::Paused,
            }
        } if *event_run_id == run_id
    )));
    drop(restored);
    fs::remove_file(persistence).unwrap();
    fs::remove_dir_all(session_root_base).unwrap();
}

#[test]
fn m5_workspace_context_vcs_and_task_evidence_are_authoritative() {
    let root = workspace();
    fs::write(root.join("README.md"), "fn answer() {\n TODO\n}\n").unwrap();
    let git = |arguments: &[&str]| {
        assert!(
            Command::new("git")
                .env_remove("GIT_DIR")
                .env_remove("GIT_WORK_TREE")
                .env_remove("GIT_INDEX_FILE")
                .env_remove("GIT_COMMON_DIR")
                .args(arguments)
                .current_dir(&root)
                .status()
                .unwrap()
                .success()
        );
    };
    git(&["init", "-q"]);
    git(&["config", "user.email", "loom@example.test"]);
    git(&["config", "user.name", "Loom Test"]);
    git(&["add", "--", "README.md"]);
    git(&["commit", "-qm", "initial"]);

    let backend = InProcessBackend::new();
    let connection = backend.connect();
    negotiate_m5(&connection);
    let workspace = connection.request(RequestEnvelope::new(ClientRequest::Workspace(
        WorkspaceRequest::CreateWorkspace {
            name: "Context workspace".to_owned(),
        },
    )));
    let ServerResponse::Workspace(WorkspaceResponse::WorkspaceCreated(workspace)) =
        workspace.result.unwrap()
    else {
        panic!("unexpected workspace response");
    };
    let created = connection.request(RequestEnvelope::new(ClientRequest::Workspace(
        WorkspaceRequest::CreateAgentSessionInWorkspace {
            workspace_id: workspace.id,
            name: "Context session".to_owned(),
        },
    )));
    let ServerResponse::Session(SessionResponse::AgentSessionCreated(session)) =
        created.result.unwrap()
    else {
        panic!("unexpected session response");
    };
    let session_id = session.id;
    let attached = connection.request(RequestEnvelope::new(ClientRequest::Repository(
        RepositoryRequest::AttachSessionRepository {
            session_id,
            source: root.display().to_string(),
            path: "repo".to_owned(),
            revision: None,
        },
    )));
    assert!(matches!(
        attached.result,
        Ok(ServerResponse::Repository(
            RepositoryResponse::SessionRepositoryAttached(_)
        ))
    ));
    let context = connection.request(RequestEnvelope::new(ClientRequest::Filesystem(
        FilesystemRequest::GetSessionContextFiles { session_id },
    )));
    assert!(matches!(
        context.result,
        Ok(ServerResponse::Filesystem(
            FilesystemResponse::ContextFiles { .. }
        ))
    ));
    let repositories = connection.request(RequestEnvelope::new(ClientRequest::Repository(
        RepositoryRequest::ListSessionRepositories { session_id },
    )));
    let ServerResponse::Repository(RepositoryResponse::SessionRepositories { repositories }) =
        repositories.result.unwrap()
    else {
        panic!("unexpected session repositories");
    };
    let vcs = connection.request(RequestEnvelope::new(ClientRequest::Repository(
        RepositoryRequest::GetSessionVcsStatus {
            session_id,
            repository_id: repositories[0].id,
        },
    )));
    assert!(matches!(
        vcs.result,
        Ok(ServerResponse::Repository(RepositoryResponse::VcsStatus(_)))
    ));

    let task = connection.request(RequestEnvelope::new(ClientRequest::Task(
        TaskRequest::StartSessionTask {
            session_id,
            spec: TaskSpec {
                kind: TaskKind::Test,
                label: "evidence fixture".to_owned(),
                command: if cfg!(windows) {
                    "cmd".to_owned()
                } else {
                    "printf".to_owned()
                },
                args: if cfg!(windows) {
                    vec!["/C".to_owned(), "ok".to_owned()]
                } else {
                    vec!["ok".to_owned()]
                },
                cwd: None,
                output_limit_bytes: Some(128),
                artifact_paths: Vec::new(),
            },
        },
    )));
    let task_id = match task.result.unwrap() {
        ServerResponse::Task(TaskResponse::TaskStarted(task)) => task.id,
        response => panic!("unexpected task response: {response:?}"),
    };
    for _ in 0..100 {
        let current = connection.request(RequestEnvelope::new(ClientRequest::Task(
            TaskRequest::GetSessionTask {
                session_id,
                task_id,
            },
        )));
        if let Ok(ServerResponse::Task(TaskResponse::Task(snapshot))) = current.result
            && matches!(
                snapshot.status,
                TaskStatus::Completed | TaskStatus::Failed | TaskStatus::Cancelled
            )
        {
            let evidence = connection.request(RequestEnvelope::new(ClientRequest::Task(
                TaskRequest::GetSessionTaskEvidence {
                    session_id,
                    task_id,
                },
            )));
            assert!(matches!(
                evidence.result,
                Ok(ServerResponse::Task(TaskResponse::TaskEvidence { .. }))
            ));
            fs::remove_dir_all(&backend.session_root_base).unwrap();
            fs::remove_dir_all(root).unwrap();
            return;
        }
        thread::sleep(Duration::from_millis(2));
    }
    panic!("task evidence fixture did not finish");
}

#[test]
fn workspace_reconnect_feed_isolated_and_pruned_cursors_resync_to_workspace_snapshot() {
    let path = std::env::temp_dir().join(format!(
        "loom-server-workspace-feed-{}.db",
        WorkspaceId::new()
    ));
    let (workspace_a, workspace_b, session_a, session_b, previous_epoch) = {
        let backend = InProcessBackend::new_persistent(&path).unwrap();
        backend.set_event_retention(1).unwrap();
        let connection = backend.connect();
        negotiate(&connection);
        let create_workspace = |name: &str| {
            let response = connection.request(RequestEnvelope::new(ClientRequest::Workspace(
                WorkspaceRequest::CreateWorkspace {
                    name: name.to_owned(),
                },
            )));
            let ServerResponse::Workspace(WorkspaceResponse::WorkspaceCreated(workspace)) =
                response.result.unwrap()
            else {
                panic!("unexpected workspace response");
            };
            workspace.id
        };
        let workspace_a = create_workspace("Workspace feed A");
        let workspace_b = create_workspace("Workspace feed B");
        let create_session = |workspace_id, name: &str| {
            let response = connection.request(RequestEnvelope::new(ClientRequest::Workspace(
                WorkspaceRequest::CreateAgentSessionInWorkspace {
                    workspace_id,
                    name: name.to_owned(),
                },
            )));
            let ServerResponse::Session(SessionResponse::AgentSessionCreated(session)) =
                response.result.unwrap()
            else {
                panic!("unexpected session response");
            };
            session.id
        };
        let session_a = create_session(workspace_a, "A");
        let session_b = create_session(workspace_b, "B");
        for (session_id, label) in [(session_a, "A"), (session_b, "B")] {
            for revision in 1..=2 {
                connection
                    .request(RequestEnvelope::new(ClientRequest::Session(
                        SessionRequest::RenameAgentSession {
                            session_id,
                            name: format!("{label} {revision}"),
                        },
                    )))
                    .result
                    .unwrap();
            }
        }

        for workspace_id in [workspace_a, workspace_b] {
            let response = connection.request(RequestEnvelope::new(ClientRequest::Events(
                EventsRequest::GetSessionEvents {
                    session_id: None,
                    workspace_id: Some(workspace_id),
                    after_sequence: None,
                    stream_epoch: None,
                },
            )));
            let ServerResponse::Events(EventsResponse::WorkspaceEventsSnapshot {
                workspace_id: returned_workspace,
                sessions,
                events,
                ..
            }) = response.result.unwrap()
            else {
                panic!("expected a snapshot after in-memory feed pruning");
            };
            assert_eq!(returned_workspace, workspace_id);
            assert_eq!(sessions.len(), 1);
            assert!(events.iter().all(|event| matches!(event,
                        WorkspaceFeedEvent::Session(event) if event.session_id == sessions[0].id)));
        }

        let ambiguous = connection.request(RequestEnvelope::new(ClientRequest::Events(
            EventsRequest::GetSessionEvents {
                session_id: Some(session_a),
                workspace_id: Some(workspace_a),
                after_sequence: None,
                stream_epoch: None,
            },
        )));
        assert!(ambiguous.result.is_err());
        let unknown = connection.request(RequestEnvelope::new(ClientRequest::Events(
            EventsRequest::GetSessionEvents {
                session_id: None,
                workspace_id: Some(WorkspaceId::new()),
                after_sequence: None,
                stream_epoch: None,
            },
        )));
        assert!(unknown.result.is_err());

        backend.flush().unwrap();
        let recovered = (
            workspace_a,
            workspace_b,
            session_a,
            session_b,
            backend.node_id.clone(),
        );
        backend.shutdown().unwrap();
        recovered
    };

    let backend = InProcessBackend::new_persistent(&path).unwrap();
    let connection = backend.connect();
    negotiate(&connection);
    let stale = connection.request(RequestEnvelope::new(ClientRequest::Events(
        EventsRequest::GetSessionEvents {
            session_id: None,
            workspace_id: Some(workspace_a),
            after_sequence: Some(EventSequence::new(1)),
            stream_epoch: None,
        },
    )));
    let ServerResponse::Events(EventsResponse::WorkspaceEventsSnapshot {
        workspace_id,
        sessions,
        events,
        oldest_sequence,
        latest_sequence,
        stream_epoch: Some(current_epoch),
    }) = stale.result.unwrap()
    else {
        panic!("expected a workspace snapshot after persisted pruning");
    };
    assert_eq!(workspace_id, workspace_a);
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0].id, session_a);
    assert!(events.iter().all(|event| matches!(event,
        WorkspaceFeedEvent::Session(event) if event.session_id == session_a)));
    assert!(!events.iter().any(|event| matches!(event,
        WorkspaceFeedEvent::Session(event) if event.session_id == session_b)));
    assert!(oldest_sequence <= latest_sequence);
    assert_ne!(current_epoch, previous_epoch);

    let events_b = connection.request(RequestEnvelope::new(ClientRequest::Events(
        EventsRequest::GetSessionEvents {
            session_id: None,
            workspace_id: Some(workspace_b),
            after_sequence: None,
            stream_epoch: None,
        },
    )));
    let ServerResponse::Events(EventsResponse::WorkspaceEventsSnapshot {
        sessions,
        events,
        latest_sequence: global_cursor,
        stream_epoch: Some(current_epoch),
        ..
    }) = events_b.result.unwrap()
    else {
        panic!("expected a workspace snapshot after persisted pruning");
    };
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0].id, session_b);
    assert!(events.iter().all(|event| matches!(event,
        WorkspaceFeedEvent::Session(event) if event.session_id == session_b)));

    let advanced_cursor = connection.request(RequestEnvelope::new(ClientRequest::Events(
        EventsRequest::GetSessionEvents {
            session_id: None,
            workspace_id: Some(workspace_a),
            after_sequence: Some(global_cursor),
            stream_epoch: Some(current_epoch),
        },
    )));
    assert!(matches!(
        advanced_cursor.result.unwrap(),
        ServerResponse::Events(EventsResponse::WorkspaceEvents{ events, .. }) if events.is_empty()
    ));

    fs::remove_file(path).unwrap();
}

#[test]
fn workspace_rename_and_config_changes_are_durable_workspace_events() {
    let path = std::env::temp_dir().join(format!(
        "loom-server-workspace-mutations-{}.db",
        WorkspaceId::new()
    ));
    let workspace_id = {
        let backend = InProcessBackend::new_persistent(&path).unwrap();
        let connection = backend.connect();
        negotiate(&connection);
        let created = connection.request(RequestEnvelope::new(ClientRequest::Workspace(
            WorkspaceRequest::CreateWorkspace {
                name: "Before rename".to_owned(),
            },
        )));
        let ServerResponse::Workspace(WorkspaceResponse::WorkspaceCreated(workspace)) =
            created.result.unwrap()
        else {
            panic!("unexpected workspace response");
        };
        connection
            .request(RequestEnvelope::new(ClientRequest::Workspace(
                WorkspaceRequest::RenameWorkspace {
                    workspace_id: workspace.id,
                    name: "After rename".to_owned(),
                },
            )))
            .result
            .unwrap();
        let config = WorkspaceConfig {
            revision: 7,
            ..WorkspaceConfig::default()
        };
        connection
            .request(RequestEnvelope::new(ClientRequest::Workspace(
                WorkspaceRequest::SetWorkspaceConfigForWorkspace {
                    workspace_id: workspace.id,
                    config,
                },
            )))
            .result
            .unwrap();
        backend.flush().unwrap();
        let workspace_id = workspace.id;
        backend.shutdown().unwrap();
        workspace_id
    };

    let backend = InProcessBackend::new_persistent(&path).unwrap();
    let connection = backend.connect();
    negotiate(&connection);
    let response = connection.request(RequestEnvelope::new(ClientRequest::Events(
        EventsRequest::GetSessionEvents {
            session_id: None,
            workspace_id: Some(workspace_id),
            after_sequence: Some(EventSequence::default()),
            stream_epoch: None,
        },
    )));
    let ServerResponse::Events(EventsResponse::WorkspaceEvents {
        workspace_id: returned,
        events,
        ..
    }) = response.result.unwrap()
    else {
        panic!("expected workspace event batch");
    };
    assert_eq!(returned, workspace_id);
    assert_eq!(events.len(), 2);
    assert!(
        matches!(events[0], WorkspaceFeedEvent::Workspace(WorkspaceEventEnvelope {
        event: WorkspaceEvent::Renamed { ref name }, ..
    }) if name == "After rename")
    );
    assert!(matches!(
        events[1],
        WorkspaceFeedEvent::Workspace(WorkspaceEventEnvelope {
            event: WorkspaceEvent::ConfigChanged { revision: 7 },
            ..
        })
    ));
    fs::remove_file(path).unwrap();
}

#[test]
fn workspace_event_requests_require_workspace_event_capability() {
    let path = std::env::temp_dir().join(format!(
        "loom-server-workspace-feed-capability-{}.db",
        WorkspaceId::new()
    ));
    let backend = InProcessBackend::new_persistent(&path).unwrap();
    let writer = backend.connect();
    negotiate(&writer);
    let created = writer.request(RequestEnvelope::new(ClientRequest::Workspace(
        WorkspaceRequest::CreateWorkspace {
            name: "Before".to_owned(),
        },
    )));
    let ServerResponse::Workspace(WorkspaceResponse::WorkspaceCreated(workspace)) =
        created.result.unwrap()
    else {
        panic!("unexpected workspace response");
    };
    writer
        .request(RequestEnvelope::new(ClientRequest::Workspace(
            WorkspaceRequest::RenameWorkspace {
                workspace_id: workspace.id,
                name: "After".to_owned(),
            },
        )))
        .result
        .unwrap();

    let limited = backend.connect();
    let limited_capabilities = CapabilitySet::new([Capability::SubscribeSessionEvents]);
    let negotiated = limited.request(RequestEnvelope::with_version(
        CURRENT_PROTOCOL_VERSION,
        ClientRequest::Control(ControlRequest::Negotiate {
            client_version: CURRENT_PROTOCOL_VERSION,
            capabilities: limited_capabilities,
        }),
    ));
    assert!(matches!(
        negotiated.result,
        Ok(ServerResponse::Control(ControlResponse::Negotiated(_)))
    ));
    let unsupported_events = limited.request(RequestEnvelope::with_version(
        CURRENT_PROTOCOL_VERSION,
        ClientRequest::Events(EventsRequest::GetSessionEvents {
            session_id: None,
            workspace_id: Some(workspace.id),
            after_sequence: Some(EventSequence::default()),
            stream_epoch: None,
        }),
    ));
    assert_eq!(
        unsupported_events.result.unwrap_err().code,
        ErrorCode::CapabilityDenied
    );

    let current = backend.connect();
    negotiate(&current);
    let current_events = current.request(RequestEnvelope::new(ClientRequest::Events(
        EventsRequest::GetSessionEvents {
            session_id: None,
            workspace_id: Some(workspace.id),
            after_sequence: Some(EventSequence::default()),
            stream_epoch: None,
        },
    )));
    assert!(matches!(current_events.result.unwrap(),
        ServerResponse::Events(EventsResponse::WorkspaceEvents{ events, .. })
            if matches!(events.as_slice(), [WorkspaceFeedEvent::Workspace(_)])));
    fs::remove_file(path).unwrap();
}
