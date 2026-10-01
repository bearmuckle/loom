//! In-process tests: runs.

#[allow(unused_imports)]
use super::support::*;
use super::*;

#[test]
fn resumable_runs_without_pending_tool_intent_are_deferred_on_restore() {
    for state in [
        AgentRunState::Planning,
        AgentRunState::Executing,
        AgentRunState::Evaluating,
        AgentRunState::AwaitingApproval,
        AgentRunState::NeedsInput,
        AgentRunState::Paused,
    ] {
        assert!(run_can_be_deferred_during_restore(
            state,
            Some(false),
            Some(false)
        ));
        assert!(!run_can_be_deferred_during_restore(
            state,
            Some(true),
            Some(false)
        ));
        assert!(!run_can_be_deferred_during_restore(
            state,
            None,
            Some(false)
        ));
    }
    for state in [
        AgentRunState::Completed,
        AgentRunState::Failed,
        AgentRunState::Cancelled,
    ] {
        assert!(!run_can_be_deferred_during_restore(
            state,
            Some(false),
            Some(false)
        ));
    }
}

#[test]
fn runs_deterministic_agent_through_approvals() {
    let root = git_repository();
    let backend = InProcessBackend::new();
    let connection = backend.connect();
    negotiate(&connection);
    let workspace = connection.request(RequestEnvelope::new(ClientRequest::Workspace(
        WorkspaceRequest::CreateWorkspace {
            name: "M1 workspace".to_owned(),
        },
    )));
    let ServerResponse::Workspace(WorkspaceResponse::WorkspaceCreated(workspace)) =
        workspace.result.unwrap()
    else {
        panic!("unexpected workspace response");
    };
    let session = connection.request(RequestEnvelope::new(ClientRequest::Workspace(
        WorkspaceRequest::CreateAgentSessionInWorkspace {
            workspace_id: workspace.id,
            name: "M1 run".to_owned(),
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
    let attached = connection.request(RequestEnvelope::new(ClientRequest::Repository(
        RepositoryRequest::AttachSessionRepository {
            session_id,
            source: root.display().to_string(),
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
        "{:?}",
        attached.result
    );
    let started = connection.request(RequestEnvelope::new(ClientRequest::Run(
        RunRequest::StartSessionAgentRun {
            session_id,
            task: "create a demo file".to_owned(),
            model: ModelId::new("deterministic/demo"),
            system_instructions: Some("Be concise.".to_owned()),
            repository_instructions: Some("Keep changes focused.".to_owned()),
        },
    )));
    let run_id = match started.result.unwrap() {
        ServerResponse::Run(RunResponse::AgentRunStarted(snapshot)) => snapshot.id,
        response => panic!("unexpected response: {response:?}"),
    };

    let mut after = None;
    loop {
        let response = connection.request(RequestEnvelope::new(ClientRequest::Events(
            EventsRequest::GetSessionEvents {
                session_id: Some(session_id),
                workspace_id: None,
                after_sequence: after,
                stream_epoch: None,
            },
        )));
        let ServerResponse::Events(EventsResponse::SessionEvents { events, .. }) =
            response.result.unwrap()
        else {
            panic!("unexpected response");
        };
        let mut completed = false;
        for event in &events {
            after = Some(event.sequence);
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
            {
                assert_eq!(*event_run, run_id);
                let response = connection.request(RequestEnvelope::new(ClientRequest::Run(
                    RunRequest::ApproveAgentAction {
                        run_id,
                        attempt_id: *attempt_id,
                        expected_control_revision: *control_revision,
                        tool_call_id: call.id,
                    },
                )));
                assert!(response.result.is_ok());
            }
            if matches!(
                &event.event,
                ServerEvent::Agent {
                    event: AgentEvent::RunCompleted { .. }
                }
            ) {
                completed = true;
            }
        }
        if completed {
            break;
        }
    }
    let final_run = connection.request(RequestEnvelope::new(ClientRequest::Run(
        RunRequest::GetAgentRun { run_id },
    )));
    let ServerResponse::Run(RunResponse::AgentRun(snapshot)) = final_run.result.unwrap() else {
        panic!("unexpected response");
    };
    assert_eq!(snapshot.state, AgentRunState::Completed);
    let page = connection.request(RequestEnvelope::new(ClientRequest::Run(
        RunRequest::GetAgentRunMessagePage {
            run_id,
            before_ordinal: None,
            limit: 2,
        },
    )));
    let ServerResponse::Run(RunResponse::AgentRunMessagePage { messages, .. }) =
        page.result.unwrap()
    else {
        panic!("unexpected run message page response");
    };
    let oldest_ordinal = messages.last().unwrap().ordinal;
    let previous_page = connection.request(RequestEnvelope::new(ClientRequest::Run(
        RunRequest::GetAgentRunMessagePage {
            run_id,
            before_ordinal: Some(oldest_ordinal),
            limit: 1,
        },
    )));
    let ServerResponse::Run(RunResponse::AgentRunMessagePage {
        messages: previous_messages,
        ..
    }) = previous_page.result.unwrap()
    else {
        panic!("unexpected previous run message page response");
    };
    assert!(
        previous_messages
            .iter()
            .all(|message| message.ordinal < oldest_ordinal)
    );
    let header = messages
        .iter()
        .find(|message| message.content_bytes > 0)
        .unwrap();
    let length = header.content_bytes.min(32) as u32;
    let content = connection.request(RequestEnvelope::new(ClientRequest::Run(
        RunRequest::GetAgentRunMessageContentRange {
            run_id,
            message_ordinal: header.ordinal,
            byte_offset: 0,
            length,
        },
    )));
    let ServerResponse::Run(RunResponse::AgentRunMessageContentRange { content, .. }) =
        content.result.unwrap()
    else {
        panic!("unexpected run message content response");
    };
    assert_eq!(content.len(), length as usize);
    let beyond_content = connection.request(RequestEnvelope::new(ClientRequest::Run(
        RunRequest::GetAgentRunMessageContentRange {
            run_id,
            message_ordinal: header.ordinal,
            byte_offset: u64::MAX,
            length: 8,
        },
    )));
    assert!(matches!(
        beyond_content.result,
        Ok(ServerResponse::Run(RunResponse::AgentRunMessageContentRange{ content, .. })) if content.is_empty()
    ));
    let missing_message = connection.request(RequestEnvelope::new(ClientRequest::Run(
        RunRequest::GetAgentRunMessageContentRange {
            run_id,
            message_ordinal: oldest_ordinal + 1000,
            byte_offset: 0,
            length: 8,
        },
    )));
    assert_eq!(
        missing_message.result.unwrap_err().code,
        ErrorCode::NotFound
    );
    let empty_page = connection.request(RequestEnvelope::new(ClientRequest::Run(
        RunRequest::GetAgentRunMessagePage {
            run_id,
            before_ordinal: Some(0),
            limit: 1,
        },
    )));
    assert!(matches!(
        empty_page.result,
        Ok(ServerResponse::Run(RunResponse::AgentRunMessagePage{ messages, .. })) if messages.is_empty()
    ));
    let history = connection.request(RequestEnvelope::new(ClientRequest::Events(
        EventsRequest::GetSessionEvents {
            session_id: Some(session_id),
            workspace_id: None,
            after_sequence: None,
            stream_epoch: None,
        },
    )));
    let ServerResponse::Events(EventsResponse::SessionEvents { events, .. }) =
        history.result.unwrap()
    else {
        panic!("unexpected history response");
    };
    assert!(events.iter().any(|event| {
        matches!(
            &event.event,
            ServerEvent::Agent {
                event: AgentEvent::ActivityRecorded { activity, .. }
            } if activity.run_id == run_id && activity.completed_at.is_some()
        )
    }));
    assert!(
        backend
            .session_root_base
            .join(workspace.id.to_string())
            .join(session_id.to_string())
            .join("fs/loom-m1-demo.txt")
            .is_file()
    );
    fs::remove_dir_all(&backend.session_root_base).unwrap();
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn policy_decisions_are_visible_and_can_stop_agent_writes() {
    let backend = InProcessBackend::new();
    let connection = backend.connect();
    negotiate_m2(&connection);
    let workspace = connection.request(RequestEnvelope::new(ClientRequest::Workspace(
        WorkspaceRequest::CreateWorkspace {
            name: "Policy workspace".to_owned(),
        },
    )));
    let ServerResponse::Workspace(WorkspaceResponse::WorkspaceCreated(workspace)) =
        workspace.result.unwrap()
    else {
        panic!("unexpected workspace response");
    };
    let session = connection.request(RequestEnvelope::new(ClientRequest::Workspace(
        WorkspaceRequest::CreateAgentSessionInWorkspace {
            workspace_id: workspace.id,
            name: "M2 policy".to_owned(),
        },
    )));
    let session_id = match session.result.unwrap() {
        ServerResponse::Session(SessionResponse::AgentSessionCreated(snapshot)) => snapshot.id,
        response => panic!("unexpected response: {response:?}"),
    };
    let default_settings = connection.request(RequestEnvelope::new(ClientRequest::Session(
        SessionRequest::GetAgentSessionSnapshot { session_id },
    )));
    let ServerResponse::Session(SessionResponse::AgentSessionSnapshot(default_settings)) =
        default_settings.result.unwrap()
    else {
        panic!("unexpected session snapshot response");
    };
    assert!(default_settings.auto_approve_actions);
    assert_eq!(
        default_settings.approval_policy,
        loom_core::ApprovalPolicy::auto_approve()
    );
    let other_session = connection.request(RequestEnvelope::new(ClientRequest::Workspace(
        WorkspaceRequest::CreateAgentSessionInWorkspace {
            workspace_id: workspace.id,
            name: "Other session".to_owned(),
        },
    )));
    let other_session_id = match other_session.result.unwrap() {
        ServerResponse::Session(SessionResponse::AgentSessionCreated(snapshot)) => snapshot.id,
        response => panic!("unexpected response: {response:?}"),
    };
    let policy = loom_core::ApprovalPolicy {
        write: PolicyDecision::Deny,
        ..Default::default()
    };
    let policy_response = connection.request(RequestEnvelope::new(ClientRequest::Session(
        SessionRequest::SetSessionApprovalPolicy {
            session_id,
            policy,
            auto_approve_actions: Some(false),
        },
    )));
    assert!(matches!(
        policy_response.result,
        Ok(ServerResponse::Session(SessionResponse::ApprovalPolicy(_)))
    ));
    let other_settings = connection.request(RequestEnvelope::new(ClientRequest::Session(
        SessionRequest::GetAgentSessionSnapshot {
            session_id: other_session_id,
        },
    )));
    let ServerResponse::Session(SessionResponse::AgentSessionSnapshot(other_settings)) =
        other_settings.result.unwrap()
    else {
        panic!("unexpected session snapshot response");
    };
    assert!(other_settings.auto_approve_actions);
    assert_eq!(
        other_settings.approval_policy,
        loom_core::ApprovalPolicy::auto_approve()
    );
    let started = connection.request(RequestEnvelope::new(ClientRequest::Run(
        RunRequest::StartSessionAgentRun {
            session_id,
            task: "attempt a write".to_owned(),
            model: ModelId::new("deterministic/demo"),
            system_instructions: None,
            repository_instructions: None,
        },
    )));
    let run_id = match started.result.unwrap() {
        ServerResponse::Run(RunResponse::AgentRunStarted(snapshot)) => snapshot.id,
        response => panic!("unexpected response: {response:?}"),
    };
    await_settled_run(&connection, run_id);
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
        panic!("unexpected session event response");
    };
    assert!(events.iter().any(|event| {
        matches!(
            &event.event,
            ServerEvent::Agent {
                event: loom_agent::AgentEvent::ToolPolicyEvaluated {
                    run_id: event_run,
                    evaluation,
                    ..
                }
            } if *event_run == run_id && evaluation.decision == PolicyDecision::Deny
        )
    }));
    let run = connection.request(RequestEnvelope::new(ClientRequest::Run(
        RunRequest::GetAgentRun { run_id },
    )));
    let ServerResponse::Run(RunResponse::AgentRun(snapshot)) = run.result.unwrap() else {
        panic!("unexpected run response");
    };
    assert_eq!(snapshot.state, AgentRunState::AwaitingApproval);
    fs::remove_dir_all(&backend.session_root_base).unwrap();
}

#[test]
fn completed_runs_keep_indexed_summaries_without_restoring_runtime_objects() {
    let persistence =
        std::env::temp_dir().join(format!("loom-server-state-{}.db", WorkspaceId::new()));
    let (run_id, session_root_base) = {
        let backend = InProcessBackend::new_persistent(&persistence).unwrap();
        let session_root_base = backend.session_root_base.clone();
        let connection = backend.connect();
        negotiate_m3(&connection);
        let workspace = connection.request(RequestEnvelope::new(ClientRequest::Workspace(
            WorkspaceRequest::CreateWorkspace {
                name: "Run summary workspace".to_owned(),
            },
        )));
        let ServerResponse::Workspace(WorkspaceResponse::WorkspaceCreated(workspace)) =
            workspace.result.unwrap()
        else {
            panic!("unexpected workspace response");
        };
        let session = connection.request(RequestEnvelope::new(ClientRequest::Workspace(
            WorkspaceRequest::CreateAgentSessionInWorkspace {
                workspace_id: workspace.id,
                name: "Completed history".to_owned(),
            },
        )));
        let ServerResponse::Session(SessionResponse::AgentSessionCreated(session)) =
            session.result.unwrap()
        else {
            panic!("unexpected session response");
        };
        let started = connection.request(RequestEnvelope::new(ClientRequest::Run(
            RunRequest::StartSessionAgentRun {
                session_id: session.id,
                task: "answer briefly".to_owned(),
                model: ModelId::new("deterministic/demo"),
                system_instructions: None,
                repository_instructions: None,
            },
        )));
        let run_id = match started.result.unwrap() {
            ServerResponse::Run(RunResponse::AgentRunStarted(snapshot)) => snapshot.id,
            response => panic!("unexpected response: {response:?}"),
        };
        let settled = await_settled_run(&connection, run_id);
        assert_eq!(settled.state, AgentRunState::Completed);
        backend.flush().unwrap();
        backend.shutdown().unwrap();
        (run_id, session_root_base)
    };

    let backend = InProcessBackend::new_persistent(&persistence).unwrap();
    assert!(backend.runs().unwrap().is_empty());
    assert!(backend.persisted_runs().unwrap().is_empty());
    let connection = backend.connect();
    negotiate_m3(&connection);
    let response = connection.request(RequestEnvelope::new(ClientRequest::Run(
        RunRequest::GetAgentRun { run_id },
    )));
    assert!(matches!(
        response.result,
        Ok(ServerResponse::Run(RunResponse::AgentRun(snapshot))) if snapshot.state == AgentRunState::Completed
    ));
    let projection = connection.request(RequestEnvelope::new(ClientRequest::Run(
        RunRequest::GetAgentRunSnapshot { run_id },
    )));
    assert!(matches!(
        projection.result,
        Ok(ServerResponse::Run(RunResponse::AgentRunSnapshot(snapshot)))
            if snapshot.run.state == AgentRunState::Completed && !snapshot.messages.is_empty()
    ));
    assert!(backend.runs().unwrap().is_empty());
    fs::remove_dir_all(session_root_base).unwrap();
    let _ = fs::remove_file(persistence);
}

#[test]
fn user_direction_queues_while_the_run_is_executing() {
    let temp = std::env::temp_dir().join(format!(
        "loom-active-direction-e2e-{}",
        uuid::Uuid::new_v4()
    ));
    fs::create_dir_all(&temp).unwrap();
    let persistence_path = temp.join("state.sqlite");
    let mut model = ScriptedOpenAiEndpoint::start();
    let model_endpoint = model.endpoint.clone();
    let model_id = ModelId::new("fixture/active-direction");
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
                name: "Active direction e2e".to_owned(),
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
                task: "Summarize the repository layout.".to_owned(),
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

    // The first model request is in flight, so the run is executing. A queued
    // direction must not error.
    let active_turn = model.next_for_manager();
    let snapshot = match connection
        .request(RequestEnvelope::new(ClientRequest::Run(
            RunRequest::GetAgentRun {
                run_id: root_run_id,
            },
        )))
        .result
        .unwrap()
    {
        ServerResponse::Run(RunResponse::AgentRun(snapshot)) => snapshot,
        response => panic!("unexpected active run response: {response:?}"),
    };
    assert!(matches!(
        snapshot.state,
        AgentRunState::Planning | AgentRunState::Executing | AgentRunState::Evaluating
    ));
    connection
        .request(RequestEnvelope::new(ClientRequest::Run(
            RunRequest::SendAgentMessage {
                run_id: root_run_id,
                attempt_id: snapshot.attempt_id,
                expected_control_revision: snapshot.control_revision,
                message: "Focus on the crates directory.".to_owned(),
            },
        )))
        .result
        .unwrap();

    // This step runs a read tool, then the worker delivers the queued direction
    // before issuing the next model request.
    model.respond_with_tool(active_turn, "list_files", serde_json::json!({}));
    let redirected = model.next_for_manager();
    let messages = redirected.request["messages"].as_array().unwrap();
    assert!(
        messages.iter().any(|message| {
            message["role"] == "user"
                && message["content"]
                    .as_str()
                    .is_some_and(|content| content.contains("Focus on the crates directory"))
        }),
        "the queued direction should be delivered on the next model turn"
    );
    model.respond_with_text(redirected, "Repository layout summarized.");

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
fn a_running_model_call_can_be_interrupted_without_blocking_the_request() {
    let (endpoint, started) = slow_model_endpoint();
    let backend =
        InProcessBackend::with_openai_compatible(endpoint, "key", ModelId::new("slow/model"));
    let connection = backend.connect();
    negotiate_m3(&connection);
    let workspace = connection.request(RequestEnvelope::new(ClientRequest::Workspace(
        WorkspaceRequest::CreateWorkspace {
            name: "Interruptible workspace".to_owned(),
        },
    )));
    let ServerResponse::Workspace(WorkspaceResponse::WorkspaceCreated(workspace)) =
        workspace.result.unwrap()
    else {
        panic!("unexpected workspace response");
    };
    let session = connection.request(RequestEnvelope::new(ClientRequest::Workspace(
        WorkspaceRequest::CreateAgentSessionInWorkspace {
            workspace_id: workspace.id,
            name: "interruptible run".to_owned(),
        },
    )));
    let session_id = match session.result.unwrap() {
        ServerResponse::Session(SessionResponse::AgentSessionCreated(snapshot)) => snapshot.id,
        response => panic!("unexpected response: {response:?}"),
    };
    let started_run = connection.request(RequestEnvelope::new(ClientRequest::Run(
        RunRequest::StartSessionAgentRun {
            session_id,
            task: "stream for a long time".to_owned(),
            model: ModelId::new("slow/model"),
            system_instructions: None,
            repository_instructions: None,
        },
    )));
    let run_id = match started_run.result.unwrap() {
        ServerResponse::Run(RunResponse::AgentRunStarted(snapshot)) => snapshot.id,
        response => panic!("unexpected response: {response:?}"),
    };
    started
        .recv_timeout(Duration::from_secs(10))
        .expect("model stream started");

    // A second connection controls the run while the first one's model call
    // is still open.
    let observer = backend.connect();
    negotiate_m3(&observer);
    // The delta is journaled while the completion is still open, so a second
    // client sees it before the run ends.
    let mut streamed = false;
    for _ in 0..1_000 {
        let events = observer.request(RequestEnvelope::new(ClientRequest::Events(
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
        if events.iter().any(|event| {
            matches!(
                &event.event,
                ServerEvent::Agent {
                    event: AgentEvent::AssistantMessageDelta { text, .. }
                } if text == "thinking"
            )
        }) {
            streamed = true;
            break;
        }
        thread::sleep(Duration::from_millis(1));
    }
    assert!(
        streamed,
        "an assistant delta was not journaled mid-completion"
    );

    let before = Instant::now();
    let interrupted = observer.request(RequestEnvelope::new(ClientRequest::Run(
        RunRequest::InterruptAgentRun { run_id },
    )));
    let elapsed = before.elapsed();
    let ServerResponse::Run(RunResponse::AgentRun(snapshot)) = interrupted.result.unwrap() else {
        panic!("unexpected interrupt response");
    };
    assert_eq!(snapshot.state, AgentRunState::Cancelled);
    assert!(
        elapsed < Duration::from_secs(5),
        "interrupt waited {elapsed:?} for the model call"
    );
    fs::remove_dir_all(&backend.session_root_base).unwrap();
}

#[test]
fn a_running_model_call_can_be_paused_and_resumed() {
    let (endpoint, started) = slow_model_endpoint();
    let backend =
        InProcessBackend::with_openai_compatible(endpoint, "key", ModelId::new("slow/model"));
    let connection = backend.connect();
    negotiate_m3(&connection);
    let workspace = connection.request(RequestEnvelope::new(ClientRequest::Workspace(
        WorkspaceRequest::CreateWorkspace {
            name: "Pausable workspace".to_owned(),
        },
    )));
    let ServerResponse::Workspace(WorkspaceResponse::WorkspaceCreated(workspace)) =
        workspace.result.unwrap()
    else {
        panic!("unexpected workspace response");
    };
    let session = connection.request(RequestEnvelope::new(ClientRequest::Workspace(
        WorkspaceRequest::CreateAgentSessionInWorkspace {
            workspace_id: workspace.id,
            name: "pausable run".to_owned(),
        },
    )));
    let session_id = match session.result.unwrap() {
        ServerResponse::Session(SessionResponse::AgentSessionCreated(snapshot)) => snapshot.id,
        response => panic!("unexpected response: {response:?}"),
    };
    let started_run = connection.request(RequestEnvelope::new(ClientRequest::Run(
        RunRequest::StartSessionAgentRun {
            session_id,
            task: "stream for a long time".to_owned(),
            model: ModelId::new("slow/model"),
            system_instructions: None,
            repository_instructions: None,
        },
    )));
    let run_id = match started_run.result.unwrap() {
        ServerResponse::Run(RunResponse::AgentRunStarted(snapshot)) => snapshot.id,
        response => panic!("unexpected response: {response:?}"),
    };
    started
        .recv_timeout(Duration::from_secs(10))
        .expect("model stream started");
    let before = Instant::now();
    let paused = connection.request(RequestEnvelope::new(ClientRequest::Run(
        RunRequest::PauseAgentRun { run_id },
    )));
    let elapsed = before.elapsed();
    let ServerResponse::Run(RunResponse::AgentRun(snapshot)) = paused.result.unwrap() else {
        panic!("unexpected pause response");
    };
    assert_eq!(snapshot.state, AgentRunState::Paused);
    assert!(
        elapsed < Duration::from_secs(5),
        "pause waited {elapsed:?} for the model call"
    );
    let current = connection.request(RequestEnvelope::new(ClientRequest::Run(
        RunRequest::GetAgentRun { run_id },
    )));
    let ServerResponse::Run(RunResponse::AgentRun(snapshot)) = current.result.unwrap() else {
        panic!("unexpected run response");
    };
    assert_eq!(snapshot.state, AgentRunState::Paused);
    fs::remove_dir_all(&backend.session_root_base).unwrap();
}

#[test]
fn retryable_mutation_idempotency_survives_backend_restart() {
    let path =
        std::env::temp_dir().join(format!("loom-server-idempotency-{}.db", WorkspaceId::new()));
    let request_id = loom_core::RequestId::new();
    let (workspace_id, first) = {
        let backend = InProcessBackend::new_persistent(&path).unwrap();
        let connection = backend.connect();
        negotiate(&connection);
        let created = connection.request(RequestEnvelope::new(ClientRequest::Workspace(
            WorkspaceRequest::CreateWorkspace {
                name: "Idempotency workspace".to_owned(),
            },
        )));
        let ServerResponse::Workspace(WorkspaceResponse::WorkspaceCreated(workspace)) =
            created.result.unwrap()
        else {
            panic!("unexpected workspace response");
        };
        let request = RequestEnvelope::with_request_id(
            request_id,
            ClientRequest::Workspace(WorkspaceRequest::CreateAgentSessionInWorkspace {
                workspace_id: workspace.id,
                name: "durable idempotency".to_owned(),
            }),
        );
        let response = connection.request(request);
        backend.shutdown().unwrap();
        (workspace.id, response)
    };
    let backend = InProcessBackend::new_persistent(&path).unwrap();
    let connection = backend.connect();
    negotiate(&connection);
    let request = RequestEnvelope::with_request_id(
        request_id,
        ClientRequest::Workspace(WorkspaceRequest::CreateAgentSessionInWorkspace {
            workspace_id,
            name: "durable idempotency".to_owned(),
        }),
    );
    let second = connection.request(request);
    assert_eq!(first, second);
    assert!(matches!(
        first.result,
        Ok(ServerResponse::Session(
            SessionResponse::AgentSessionCreated(_)
        ))
    ));
    let expired_request_id = request_id_with_issued_at(
        current_unix_millis()
            .saturating_sub(IDEMPOTENCY_RETENTION.as_millis() as u64)
            .saturating_sub(1),
    );
    let expired = connection.request(RequestEnvelope::with_request_id(
        expired_request_id,
        ClientRequest::Workspace(WorkspaceRequest::CreateWorkspace {
            name: "must not be replayed".to_owned(),
        }),
    ));
    assert_eq!(
        expired.result.unwrap_err().code,
        ErrorCode::DeadlineExceeded
    );
    let workspaces = connection.request(RequestEnvelope::new(ClientRequest::Workspace(
        WorkspaceRequest::ListWorkspaces,
    )));
    assert!(matches!(
        workspaces.result,
        Ok(ServerResponse::Workspace(WorkspaceResponse::Workspaces{ workspaces })) if workspaces.len() == 1
    ));
    fs::remove_file(path).unwrap();
}

#[test]
fn retryable_mutation_save_failure_does_not_cache_success() {
    let path = std::env::temp_dir().join(format!(
        "loom-server-idempotency-failure-{}.db",
        WorkspaceId::new()
    ));
    let backend = InProcessBackend::new_persistent(&path).unwrap();
    let connection = backend.connect();
    negotiate(&connection);
    let request_id = loom_core::RequestId::new();
    let request = ClientRequest::Workspace(WorkspaceRequest::CreateWorkspace {
        name: "failure boundary".to_owned(),
    });
    backend.fail_next_state_save.store(true, Ordering::SeqCst);
    let failed = connection.request(RequestEnvelope::with_request_id(
        request_id,
        request.clone(),
    ));
    assert_eq!(failed.result.unwrap_err().code, ErrorCode::Internal);
    assert!(
        backend
            .idempotency_store
            .cached_response(request_id, &request)
            .unwrap()
            .is_none()
    );

    // Handler state can already have changed when a save fails. Fail-stop
    // prevents a retry from dispatching against that partially mutated
    // in-memory state; reopening restores the last committed disk state.
    let retried = connection.request(RequestEnvelope::with_request_id(request_id, request));
    assert_eq!(retried.result.unwrap_err().code, ErrorCode::Persistence);
    let listed = connection.request(RequestEnvelope::new(ClientRequest::Workspace(
        WorkspaceRequest::ListWorkspaces,
    )));
    assert_eq!(listed.result.unwrap_err().code, ErrorCode::Persistence);
    backend.shutdown().unwrap();
    drop(connection);
    drop(backend);
    let reopened = InProcessBackend::new_persistent(&path).unwrap();
    let connection = reopened.connect();
    negotiate(&connection);
    let listed = connection.request(RequestEnvelope::new(ClientRequest::Workspace(
        WorkspaceRequest::ListWorkspaces,
    )));
    assert!(
        matches!(listed.result, Ok(ServerResponse::Workspace(WorkspaceResponse::Workspaces{ workspaces })) if workspaces.is_empty())
    );
    drop(connection);
    drop(reopened);
    fs::remove_file(path).unwrap();
}

#[test]
fn transcript_page_content_is_bounded_and_marks_truncation() {
    let large = vec![b'x'; MAX_AGENT_RUN_TRANSCRIPT_MESSAGE_BYTES as usize + 1];
    let (content, truncated) = bounded_transcript_content(
        &large[..MAX_AGENT_RUN_TRANSCRIPT_MESSAGE_BYTES as usize],
        MAX_AGENT_RUN_TRANSCRIPT_MESSAGE_BYTES as u64 + 1,
    );
    assert!(truncated);
    assert!(content.starts_with(&"x".repeat(MAX_AGENT_RUN_TRANSCRIPT_MESSAGE_BYTES as usize)));
    assert!(content.ends_with("\n...[message truncated]"));

    let (content, truncated) = bounded_transcript_content(b"short", 5);
    assert!(!truncated);
    assert_eq!(content, "short");
    assert_eq!(bounded_transcript_content(&[], 0), (String::new(), false));
}
