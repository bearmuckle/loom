use std::{sync::Arc, time::Duration};

use futures_util::{SinkExt, StreamExt};
use loom_agent::AgentEvent;
use loom_core::{
    AgentSessionId, ApprovalPolicy, Capability, CapabilitySet, ErrorCode, EventSequence, RequestId,
    WorkspaceId,
};
use loom_model::ModelId;
use loom_protocol::{
    CURRENT_PROTOCOL_VERSION, ClientRequest, RequestEnvelope, ServerEvent, ServerResponse,
    WorkerNodeStatus, decode_response,
};
use loom_server::{
    AuthTokenStore, AuthorizationScope, InProcessBackend, RemoteServer, RemoteServerConfig,
    WebSocketConnection, WebSocketTransport,
};
use tokio_tungstenite::{
    connect_async,
    tungstenite::{Message, client::IntoClientRequest},
};

fn capabilities() -> CapabilitySet {
    CapabilitySet::new([
        Capability::CreateAgentSession,
        Capability::ReadAgentSession,
        Capability::ControlAgentSession,
        Capability::SubscribeSessionEvents,
        Capability::StartAgentRun,
        Capability::ReadAgentRun,
        Capability::ControlAgentRun,
        Capability::ApproveAgentAction,
        Capability::ConfigureApprovalPolicy,
        Capability::ListProviders,
        Capability::ManageWorkspaces,
        Capability::ManageSessionRepositories,
        Capability::ReadSessionFilesystem,
        Capability::WriteSessionFilesystem,
        Capability::ConfigureProviders,
        Capability::ReadWorkspaceConfig,
        Capability::ReadVcsStatus,
        Capability::ReadVcsDiff,
        Capability::ReadSessionTaskEvidence,
        Capability::ReadWorkerNodeStatus,
        Capability::OpenSessionTerminal,
        Capability::ControlSessionTerminal,
        Capability::JsonProtocol,
    ])
}

async fn server() -> (
    Arc<InProcessBackend>,
    Arc<AuthTokenStore>,
    loom_server::IssuedToken,
    loom_server::RunningRemoteServer,
) {
    let backend = InProcessBackend::new();
    let auth = Arc::new(AuthTokenStore::new());
    let token = auth
        .insert("remote-test-token", AuthorizationScope::all())
        .unwrap();
    let server = RemoteServer::new(
        Arc::clone(&backend),
        Arc::clone(&auth),
        RemoteServerConfig {
            heartbeat_interval: Duration::from_millis(25),
            request_timeout: Duration::from_secs(5),
            ..RemoteServerConfig::local_ephemeral()
        },
    )
    .bind()
    .await
    .unwrap();
    (backend, auth, token, server)
}

#[tokio::test]
async fn invalid_websocket_url_errors_do_not_echo_credentials() {
    let error = match WebSocketTransport::new(
        "not-a-websocket-url?access_token=url-secret",
        "header-secret",
    )
    .connect()
    .await
    {
        Ok(_) => panic!("invalid worker URL unexpectedly connected"),
        Err(error) => error,
    };

    assert!(error.message.contains("invalid WebSocket URL"));
    assert!(!error.to_string().contains("url-secret"));
    assert!(!error.to_string().contains("header-secret"));
}

async fn negotiate(connection: &mut WebSocketConnection) {
    let response = connection
        .request(RequestEnvelope::new(ClientRequest::DiscoverCapabilities))
        .await
        .unwrap();
    assert!(matches!(
        response.result,
        Ok(ServerResponse::Capabilities(_))
    ));
    let response = connection
        .request(RequestEnvelope::new(ClientRequest::Negotiate {
            client_version: CURRENT_PROTOCOL_VERSION,
            capabilities: capabilities(),
        }))
        .await
        .unwrap();
    assert!(matches!(response.result, Ok(ServerResponse::Negotiated(_))));
}

async fn create_workspace(connection: &mut WebSocketConnection) -> WorkspaceId {
    match connection
        .request(RequestEnvelope::new(ClientRequest::CreateWorkspace {
            name: "remote workspace".to_owned(),
        }))
        .await
        .unwrap()
        .result
        .unwrap()
    {
        ServerResponse::WorkspaceCreated(workspace) => workspace.id,
        response => panic!("unexpected workspace response: {response:?}"),
    }
}

async fn session(
    connection: &mut WebSocketConnection,
    workspace_id: WorkspaceId,
) -> loom_core::AgentSessionSnapshot {
    match connection
        .request(RequestEnvelope::new(
            ClientRequest::CreateAgentSessionInWorkspace {
                workspace_id,
                name: "remote session".to_owned(),
            },
        ))
        .await
        .unwrap()
        .result
        .unwrap()
    {
        ServerResponse::AgentSessionCreated(snapshot) => snapshot,
        response => panic!("unexpected session response: {response:?}"),
    }
}

async fn events(
    connection: &mut WebSocketConnection,
    session_id: AgentSessionId,
    after_sequence: Option<EventSequence>,
) -> Vec<loom_protocol::ServerEventEnvelope> {
    match connection
        .request(RequestEnvelope::new(ClientRequest::GetSessionEvents {
            session_id: Some(session_id),
            after_sequence,
        }))
        .await
        .unwrap()
        .result
        .unwrap()
    {
        ServerResponse::SessionEvents { events }
        | ServerResponse::SessionEventsSnapshot { events, .. } => events,
        response => panic!("unexpected event response: {response:?}"),
    }
}

async fn worker_status(connection: &mut WebSocketConnection) -> WorkerNodeStatus {
    match connection
        .request(RequestEnvelope::new(ClientRequest::GetWorkerNodeStatus))
        .await
        .unwrap()
        .result
        .unwrap()
    {
        ServerResponse::WorkerNodeStatus(status) => status,
        response => panic!("unexpected worker status response: {response:?}"),
    }
}

#[tokio::test]
async fn rejects_unauthenticated_websocket_clients_before_capability_discovery() {
    let (_backend, _auth, _token, server) = server().await;
    let error = match WebSocketTransport::new(server.websocket_url(), "wrong-token")
        .connect()
        .await
    {
        Ok(_) => panic!("invalid token unexpectedly connected"),
        Err(error) => error,
    };
    assert_eq!(error.code, ErrorCode::AuthenticationFailed);
    server.stop().await.unwrap();
}

#[tokio::test]
async fn authorized_client_can_discover_and_revoke_access() {
    let (_backend, auth, token, server) = server().await;
    let transport = WebSocketTransport::new(server.websocket_url(), token.token.clone());
    let mut connection = transport.connect().await.unwrap();
    negotiate(&mut connection).await;
    let discovered = connection
        .request(RequestEnvelope::new(ClientRequest::DiscoverCapabilities))
        .await
        .unwrap();
    let discovered = match discovered.result {
        Ok(ServerResponse::Capabilities(result)) => result.capabilities,
        other => panic!("unexpected capability response: {other:?}"),
    };
    assert!(discovered.contains(Capability::ReadSessionFilesystem));
    assert!(discovered.contains(Capability::ReadVcsStatus));
    let workspace_id = create_workspace(&mut connection).await;
    let _ = session(&mut connection, workspace_id).await;
    auth.revoke(&token.token_id).unwrap();
    let response = connection
        .request(RequestEnvelope::new(ClientRequest::ListModels))
        .await
        .unwrap();
    assert_eq!(
        response.result.unwrap_err().code,
        ErrorCode::AuthenticationFailed
    );
    server.stop().await.unwrap();
}

#[tokio::test]
async fn authorized_client_can_configure_copilot_on_remote_worker() {
    let (_backend, _auth, token, server) = server().await;
    let mut connection = WebSocketTransport::new(server.websocket_url(), token.token.clone())
        .connect()
        .await
        .unwrap();
    negotiate(&mut connection).await;

    let response = connection
        .request(RequestEnvelope::new(
            ClientRequest::ConfigureGitHubCopilot {
                access_token: "github-device-flow-token".to_owned(),
            },
        ))
        .await
        .unwrap();
    assert!(matches!(
        response.result,
        Ok(ServerResponse::ProviderConfigured)
    ));

    let response = connection
        .request(RequestEnvelope::new(ClientRequest::ListProviders))
        .await
        .unwrap();
    let Ok(ServerResponse::Providers { providers }) = response.result else {
        panic!("unexpected provider response: {:?}", response.result);
    };
    assert!(
        providers
            .iter()
            .any(|provider| provider.kind == loom_model::ProviderKind::GitHubCopilot)
    );

    let response = connection
        .request(RequestEnvelope::new(
            ClientRequest::GetGitHubCopilotLoginStatus {
                login_id: "missing-login".to_owned(),
            },
        ))
        .await
        .unwrap();
    assert_eq!(response.result.unwrap_err().code, ErrorCode::NotFound);
    server.stop().await.unwrap();
}

#[tokio::test]
async fn provider_configuration_requires_its_dedicated_capability() {
    let (_backend, auth, _token, server) = server().await;
    let token = auth
        .insert(
            "provider-read-only-token",
            AuthorizationScope {
                capabilities: Some(CapabilitySet::new([Capability::ListProviders])),
                ..AuthorizationScope::default()
            },
        )
        .unwrap();
    let mut connection = WebSocketTransport::new(server.websocket_url(), token.token.clone())
        .connect()
        .await
        .unwrap();
    negotiate(&mut connection).await;

    let response = connection
        .request(RequestEnvelope::new(
            ClientRequest::ConfigureGitHubCopilot {
                access_token: "github-device-flow-token".to_owned(),
            },
        ))
        .await
        .unwrap();
    assert_eq!(
        response.result.unwrap_err().code,
        ErrorCode::AuthorizationDenied
    );

    for request in [
        ClientRequest::StartGitHubCopilotLogin,
        ClientRequest::GetGitHubCopilotLoginStatus {
            login_id: "unknown".to_owned(),
        },
    ] {
        let response = connection
            .request(RequestEnvelope::new(request))
            .await
            .unwrap();
        assert_eq!(
            response.result.unwrap_err().code,
            ErrorCode::AuthorizationDenied
        );
    }
    server.stop().await.unwrap();
}

#[tokio::test]
async fn secondary_local_backend_refreshes_resources_with_distinct_stable_identity() {
    let first_backend = InProcessBackend::new();
    let first_auth = Arc::new(AuthTokenStore::new());
    let first_token = first_auth
        .insert("first-local-backend", AuthorizationScope::all())
        .unwrap();
    let first_server = RemoteServer::new(
        Arc::clone(&first_backend),
        Arc::clone(&first_auth),
        RemoteServerConfig::local_ephemeral(),
    )
    .bind()
    .await
    .unwrap();
    let second_backend = InProcessBackend::new();
    let second_auth = Arc::new(AuthTokenStore::new());
    let second_token = second_auth
        .insert("second-local-backend", AuthorizationScope::all())
        .unwrap();
    let second_server = RemoteServer::new(
        Arc::clone(&second_backend),
        Arc::clone(&second_auth),
        RemoteServerConfig::local_ephemeral(),
    )
    .bind()
    .await
    .unwrap();
    let mut first_connection =
        WebSocketTransport::new(first_server.websocket_url(), first_token.token)
            .connect()
            .await
            .unwrap();
    let mut second_connection =
        WebSocketTransport::new(second_server.websocket_url(), second_token.token)
            .connect()
            .await
            .unwrap();
    negotiate(&mut first_connection).await;
    negotiate(&mut second_connection).await;

    let first_status = worker_status(&mut first_connection).await;
    let second_initial = worker_status(&mut second_connection).await;
    assert_eq!(first_status.resources.cpu_usage_percent, None);
    assert_eq!(second_initial.resources.cpu_usage_percent, None);
    assert!(second_initial.resources.cpu_count > 0);
    assert!(
        second_initial
            .resources
            .memory_total_bytes
            .is_some_and(|bytes| bytes > 0)
    );
    assert!(
        second_initial
            .resources
            .memory_usage_percent
            .is_some_and(|percent| percent <= 100)
    );
    assert_ne!(first_status.node_id, second_initial.node_id);
    assert_ne!(first_status.name, second_initial.name);
    #[cfg(unix)]
    {
        assert!(second_initial.resources.disk_total_bytes.is_some());
        assert!(second_initial.resources.disk_available_bytes.is_some());
    }

    tokio::time::sleep(Duration::from_millis(300)).await;
    let second_refreshed = worker_status(&mut second_connection).await;
    assert!(
        second_refreshed
            .resources
            .cpu_usage_percent
            .is_some_and(|percent| percent <= 100)
    );
    assert!(
        second_refreshed
            .resources
            .memory_usage_percent
            .is_some_and(|percent| percent <= 100)
    );
    assert_eq!(second_refreshed.node_id, second_initial.node_id);
    assert_eq!(second_refreshed.name, second_initial.name);

    drop(first_connection);
    drop(second_connection);
    first_server.stop().await.unwrap();
    second_server.stop().await.unwrap();
}

#[tokio::test]
async fn workspace_scopes_restrict_session_creation() {
    let backend = InProcessBackend::new();
    let setup = backend.connect();
    setup
        .request(RequestEnvelope::new(ClientRequest::Negotiate {
            client_version: CURRENT_PROTOCOL_VERSION,
            capabilities: capabilities(),
        }))
        .result
        .unwrap();
    let allowed_workspace = match setup
        .request(RequestEnvelope::new(ClientRequest::CreateWorkspace {
            name: "allowed".to_owned(),
        }))
        .result
        .unwrap()
    {
        ServerResponse::WorkspaceCreated(workspace) => workspace.id,
        response => panic!("unexpected workspace response: {response:?}"),
    };
    let auth = Arc::new(AuthTokenStore::new());
    let token = auth
        .insert(
            "scoped-token",
            AuthorizationScope::for_workspaces(
                [allowed_workspace],
                CapabilitySet::new([
                    Capability::CreateAgentSession,
                    Capability::ReadAgentSession,
                    Capability::SubscribeSessionEvents,
                ]),
            ),
        )
        .unwrap();
    let server = RemoteServer::new(backend, auth, RemoteServerConfig::local_ephemeral())
        .bind()
        .await
        .unwrap();
    let transport = WebSocketTransport::new(server.websocket_url(), token.token);
    let mut connection = transport.connect().await.unwrap();
    negotiate(&mut connection).await;
    let denied = connection
        .request(RequestEnvelope::new(
            ClientRequest::CreateAgentSessionInWorkspace {
                workspace_id: WorkspaceId::new(),
                name: "denied".to_owned(),
            },
        ))
        .await
        .unwrap();
    assert_eq!(
        denied.result.unwrap_err().code,
        ErrorCode::AuthorizationDenied
    );
    let allowed = session(&mut connection, allowed_workspace).await;
    let denied = connection
        .request(RequestEnvelope::new(ClientRequest::GetAgentSession {
            session_id: allowed.id,
        }))
        .await
        .unwrap();
    assert!(denied.result.is_ok());
    server.stop().await.unwrap();
}

#[tokio::test]
async fn session_scopes_filter_workspace_session_listing() {
    let backend = InProcessBackend::new();
    let setup = backend.connect();
    setup
        .request(RequestEnvelope::new(ClientRequest::Negotiate {
            client_version: CURRENT_PROTOCOL_VERSION,
            capabilities: capabilities(),
        }))
        .result
        .unwrap();
    let workspace_id = match setup
        .request(RequestEnvelope::new(ClientRequest::CreateWorkspace {
            name: "scoped workspace".to_owned(),
        }))
        .result
        .unwrap()
    {
        ServerResponse::WorkspaceCreated(workspace) => workspace.id,
        response => panic!("unexpected workspace response: {response:?}"),
    };
    let create_session = || {
        setup
            .request(RequestEnvelope::new(
                ClientRequest::CreateAgentSessionInWorkspace {
                    workspace_id,
                    name: "scoped session".to_owned(),
                },
            ))
            .result
            .unwrap()
    };
    let allowed_session = match create_session() {
        ServerResponse::AgentSessionCreated(session) => session,
        response => panic!("unexpected session response: {response:?}"),
    };
    let hidden_session = match create_session() {
        ServerResponse::AgentSessionCreated(session) => session,
        response => panic!("unexpected session response: {response:?}"),
    };

    let auth = Arc::new(AuthTokenStore::new());
    let token = auth
        .insert(
            "session-scoped-token",
            AuthorizationScope::for_sessions(
                [allowed_session.id],
                CapabilitySet::new([
                    Capability::CreateAgentSession,
                    Capability::ReadAgentSession,
                    Capability::ManageWorkspaces,
                ]),
            ),
        )
        .unwrap();
    let server = RemoteServer::new(backend, auth, RemoteServerConfig::local_ephemeral())
        .bind()
        .await
        .unwrap();
    let mut connection = WebSocketTransport::new(server.websocket_url(), token.token)
        .connect()
        .await
        .unwrap();
    negotiate(&mut connection).await;

    let response = connection
        .request(RequestEnvelope::new(ClientRequest::ListWorkspaceSessions {
            workspace_id,
            include_archived: false,
        }))
        .await
        .unwrap()
        .result
        .unwrap();
    let ServerResponse::AgentSessions { sessions } = response else {
        panic!("unexpected session listing response: {response:?}");
    };
    assert_eq!(
        sessions
            .iter()
            .map(|session| session.id)
            .collect::<Vec<_>>(),
        [allowed_session.id]
    );
    assert!(
        !sessions
            .iter()
            .any(|session| session.id == hidden_session.id)
    );

    let response = connection
        .request(RequestEnvelope::new(
            ClientRequest::CreateAgentSessionInWorkspace {
                workspace_id,
                name: "unauthorized".to_owned(),
            },
        ))
        .await
        .unwrap();
    assert_eq!(
        response.result.unwrap_err().code,
        ErrorCode::AuthorizationDenied
    );
    server.stop().await.unwrap();
}

#[tokio::test]
async fn reconnect_resumes_journal_and_approves_a_run_after_disconnect() {
    let (_backend, _auth, token, server) = server().await;
    let transport = WebSocketTransport::new(server.websocket_url(), token.token);
    let mut first = transport.connect().await.unwrap();
    negotiate(&mut first).await;
    let workspace_id = create_workspace(&mut first).await;
    let session = session(&mut first, workspace_id).await;
    first
        .request(RequestEnvelope::new(
            ClientRequest::SetSessionApprovalPolicy {
                session_id: session.id,
                policy: ApprovalPolicy::default(),
                auto_approve_actions: Some(false),
            },
        ))
        .await
        .unwrap()
        .result
        .unwrap();
    let run_id = match first
        .request(RequestEnvelope::new(ClientRequest::StartSessionAgentRun {
            session_id: session.id,
            task: "remote durable task".to_owned(),
            model: ModelId::new("deterministic/demo"),
            system_instructions: None,
            repository_instructions: None,
        }))
        .await
        .unwrap()
        .result
        .unwrap()
    {
        ServerResponse::AgentRunStarted(snapshot) => snapshot.id,
        response => panic!("unexpected run response: {response:?}"),
    };
    drop(first);

    let mut second = transport.connect().await.unwrap();
    negotiate(&mut second).await;
    let mut after = None;
    let mut approvals = 0;
    let mut completed = false;
    for _ in 0..100 {
        let batch = events(&mut second, session.id, after).await;
        if batch.is_empty() {
            tokio::time::sleep(Duration::from_millis(5)).await;
            continue;
        }
        for event in batch {
            after = Some(event.sequence);
            if let ServerEvent::Agent {
                event:
                    AgentEvent::ToolApprovalRequired {
                        run_id: event_run,
                        call,
                    },
            } = &event.event
                && *event_run == run_id
            {
                approvals += 1;
                second
                    .request(RequestEnvelope::new(ClientRequest::ApproveAgentAction {
                        run_id,
                        tool_call_id: call.id,
                    }))
                    .await
                    .unwrap()
                    .result
                    .unwrap();
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
    assert!(approvals >= 2);
    assert!(completed);
    let run = second
        .request(RequestEnvelope::new(ClientRequest::GetAgentRun { run_id }))
        .await
        .unwrap();
    assert!(matches!(
        run.result.unwrap(),
        ServerResponse::AgentRun(snapshot)
            if snapshot.state == loom_agent::AgentRunState::Completed
    ));
    server.stop().await.unwrap();
}

#[tokio::test]
async fn stale_cursors_return_a_snapshot_fallback() {
    let (backend, _auth, token, server) = server().await;
    backend.set_event_retention(2).unwrap();
    let transport = WebSocketTransport::new(server.websocket_url(), token.token);
    let mut connection = transport.connect().await.unwrap();
    negotiate(&mut connection).await;
    let workspace_id = create_workspace(&mut connection).await;
    let first = session(&mut connection, workspace_id).await;
    let _second = session(&mut connection, workspace_id).await;
    let _third = session(&mut connection, workspace_id).await;
    let response = connection
        .request(RequestEnvelope::new(ClientRequest::GetSessionEvents {
            session_id: Some(first.id),
            after_sequence: Some(EventSequence::default()),
        }))
        .await
        .unwrap();
    match response.result.unwrap() {
        ServerResponse::SessionEventsSnapshot {
            session,
            events,
            oldest_sequence,
            latest_sequence,
        } => {
            assert_eq!(session.id, first.id);
            assert!(events.is_empty());
            assert!(oldest_sequence.value() > 1);
            assert_eq!(latest_sequence.value(), 3);
        }
        response => panic!("expected snapshot fallback, got {response:?}"),
    }
    server.stop().await.unwrap();
}

#[tokio::test]
async fn duplicate_mutation_request_ids_are_idempotent() {
    let (_backend, _auth, token, server) = server().await;
    let transport = WebSocketTransport::new(server.websocket_url(), token.token);
    let mut connection = transport.connect().await.unwrap();
    negotiate(&mut connection).await;
    let workspace_id = create_workspace(&mut connection).await;
    let request_id = RequestId::new();
    let request = RequestEnvelope::with_request_id(
        request_id,
        ClientRequest::CreateAgentSessionInWorkspace {
            workspace_id,
            name: "idempotent".to_owned(),
        },
    );
    let first = connection.request(request.clone()).await.unwrap();
    drop(connection);
    let mut reconnected = transport.connect().await.unwrap();
    negotiate(&mut reconnected).await;
    let second = reconnected.request(request).await.unwrap();
    assert_eq!(first, second);
    let ServerResponse::AgentSessionCreated(snapshot) = first.result.unwrap() else {
        panic!("unexpected idempotent response");
    };
    let events = events(&mut reconnected, snapshot.id, None).await;
    assert_eq!(events.len(), 1);
    server.stop().await.unwrap();
}

#[tokio::test]
async fn malformed_payloads_receive_structured_errors() {
    let (_backend, _auth, token, server) = server().await;
    let transport = WebSocketTransport::new(server.websocket_url(), token.token);
    let mut request = transport.url().into_client_request().unwrap();
    request
        .headers_mut()
        .insert("authorization", "Bearer remote-test-token".parse().unwrap());
    let (mut socket, _) = connect_async(request).await.unwrap();
    socket
        .send(Message::Text("{not-json".to_owned().into()))
        .await
        .unwrap();
    let response = loop {
        match socket.next().await {
            Some(Ok(Message::Text(response))) => break response,
            Some(Ok(Message::Ping(payload))) => {
                socket.send(Message::Pong(payload)).await.unwrap();
            }
            Some(Ok(Message::Pong(_))) => {}
            Some(Ok(Message::Binary(_))) => {}
            Some(Ok(Message::Frame(_))) => {}
            Some(Ok(Message::Close(_))) | Some(Err(_)) | None => {
                panic!("server did not return malformed payload response");
            }
        }
    };
    let response = decode_response(response.as_bytes()).unwrap();
    assert_eq!(
        response.result.unwrap_err().code,
        ErrorCode::MalformedPayload
    );
    server.stop().await.unwrap();
}

#[tokio::test]
async fn terminal_execution_continues_after_client_disconnect() {
    let (_backend, _auth, token, server) = server().await;
    let transport = WebSocketTransport::new(server.websocket_url(), token.token);
    let mut first = transport.connect().await.unwrap();
    negotiate(&mut first).await;
    let workspace_id = create_workspace(&mut first).await;
    let session = session(&mut first, workspace_id).await;
    let terminal = match first
        .request(RequestEnvelope::new(ClientRequest::OpenSessionTerminal {
            session_id: session.id,
            command: "sh".to_owned(),
            args: vec!["-c".to_owned(), "sleep 0.05; printf done".to_owned()],
            cwd: None,
        }))
        .await
        .unwrap()
        .result
        .unwrap()
    {
        ServerResponse::TerminalOpened(snapshot) => snapshot,
        response => panic!("unexpected terminal response: {response:?}"),
    };
    drop(first);

    let mut second = transport.connect().await.unwrap();
    negotiate(&mut second).await;
    let mut exited = false;
    for _ in 0..100 {
        let response = second
            .request(RequestEnvelope::new(
                ClientRequest::GetSessionTerminalEvents {
                    session_id: session.id,
                    terminal_id: terminal.id,
                    after_sequence: None,
                },
            ))
            .await
            .unwrap();
        let ServerResponse::TerminalEvents { events } = response.result.unwrap() else {
            panic!("unexpected terminal event response");
        };
        if events
            .iter()
            .any(|event| matches!(event.event, loom_process::TerminalEvent::Exited { .. }))
        {
            exited = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert!(exited);
    server.stop().await.unwrap();
}
