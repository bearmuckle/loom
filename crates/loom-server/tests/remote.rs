use std::{collections::HashMap, sync::Arc, time::Duration};

use futures_util::{SinkExt, StreamExt};
use loom_agent::AgentEvent;
use loom_core::{
    AgentSessionId, ApprovalPolicy, Capability, CapabilitySet, ErrorCode, EventSequence, LoomError,
    RequestId, WorkspaceId,
};
use loom_model::ModelId;
use loom_protocol::{
    AccessRequest, AccessResponse, ClientRequest, ControlRequest, ControlResponse, EventsRequest,
    EventsResponse, ProviderRequest, ProviderResponse, RepositoryRequest, RequestEnvelope,
    ResponseEnvelope, RunRequest, RunResponse, ServerEvent, ServerResponse, SessionRequest,
    SessionResponse, TerminalRequest, TerminalResponse, WorkerNodeStatus, WorkspaceRequest,
    WorkspaceResponse, decode_request, decode_response, encode_request, encode_response,
};
use loom_server::{
    AuthTokenStore, AuthorizationScope, DEFAULT_MAX_FRAME_BYTES,
    DEFAULT_MAX_IN_FLIGHT_CONTROL_REQUESTS, DEFAULT_MAX_IN_FLIGHT_REQUESTS, GitHubSignInConfig,
    InProcessBackend, RemoteServer, RemoteServerConfig, ServerTlsConfig, WEBSOCKET_TLS_SUPPORTED,
    WebSocketConnection, WebSocketTransport,
};
use tokio::net::{TcpListener, TcpStream};
use tokio_tungstenite::{
    MaybeTlsStream, WebSocketStream, accept_async, connect_async,
    tungstenite::{Message, client::IntoClientRequest},
};

mod support;

/// Test-only TLS fixtures: a CA and a leaf for `127.0.0.1`/`localhost` that stay
/// valid until 2126. They exist so the end-to-end TLS test runs without network
/// access or a real certificate authority, and they protect nothing.
const TEST_CERT_PEM: &[u8] = include_bytes!("fixtures/tls/server-cert.pem");
const TEST_KEY_PEM: &[u8] = include_bytes!("fixtures/tls/server-key.pem");
const TEST_CA_PEM: &[u8] = include_bytes!("fixtures/tls/ca-cert.pem");

async fn server() -> (
    Arc<InProcessBackend>,
    Arc<AuthTokenStore>,
    loom_server::IssuedToken,
    loom_server::RunningRemoteServer,
) {
    server_with_backend(InProcessBackend::new()).await
}

async fn server_with_backend(
    backend: Arc<InProcessBackend>,
) -> (
    Arc<InProcessBackend>,
    Arc<AuthTokenStore>,
    loom_server::IssuedToken,
    loom_server::RunningRemoteServer,
) {
    server_with_backend_and_frame_limit(backend, DEFAULT_MAX_FRAME_BYTES).await
}

/// The standard test server with `max_frame_bytes` as its inbound limit.
async fn server_with_frame_limit(
    max_frame_bytes: usize,
) -> (
    Arc<InProcessBackend>,
    Arc<AuthTokenStore>,
    loom_server::IssuedToken,
    loom_server::RunningRemoteServer,
) {
    server_with_backend_and_frame_limit(InProcessBackend::new(), max_frame_bytes).await
}

async fn server_with_backend_and_frame_limit(
    backend: Arc<InProcessBackend>,
    max_frame_bytes: usize,
) -> (
    Arc<InProcessBackend>,
    Arc<AuthTokenStore>,
    loom_server::IssuedToken,
    loom_server::RunningRemoteServer,
) {
    server_with_limits(
        backend,
        max_frame_bytes,
        DEFAULT_MAX_IN_FLIGHT_REQUESTS,
        DEFAULT_MAX_IN_FLIGHT_CONTROL_REQUESTS,
    )
    .await
}

/// The standard test server with explicit per-connection in-flight limits.
async fn server_with_limits(
    backend: Arc<InProcessBackend>,
    max_frame_bytes: usize,
    max_in_flight_requests: usize,
    max_in_flight_control_requests: usize,
) -> (
    Arc<InProcessBackend>,
    Arc<AuthTokenStore>,
    loom_server::IssuedToken,
    loom_server::RunningRemoteServer,
) {
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
            max_frame_bytes,
            max_in_flight_requests,
            max_in_flight_control_requests,
            ..RemoteServerConfig::local_ephemeral()
        },
    )
    .bind()
    .await
    .unwrap();
    (backend, auth, token, server)
}

/// A request padded to about `bytes` so a frame-size bound can be exercised
/// without creating the session it names. An unknown session is answered with
/// `not_found`, which shows the frame reached the request handler.
fn padding_request(bytes: usize) -> RequestEnvelope {
    RequestEnvelope::new(ClientRequest::Session(SessionRequest::RenameAgentSession {
        session_id: AgentSessionId::new(),
        name: "p".repeat(bytes),
    }))
}

/// Opens a raw WebSocket to the test server, authenticated like a client but
/// without a client-side frame limit of its own.
async fn raw_socket(
    server: &loom_server::RunningRemoteServer,
    token: &str,
) -> WebSocketStream<MaybeTlsStream<TcpStream>> {
    let mut request = server.websocket_url().into_client_request().unwrap();
    request
        .headers_mut()
        .insert("authorization", format!("Bearer {token}").parse().unwrap());
    let (socket, _) = connect_async(request).await.unwrap();
    socket
}

/// The next text frame from `socket`, answering heartbeats in between.
async fn next_text_frame(socket: &mut WebSocketStream<MaybeTlsStream<TcpStream>>) -> Vec<u8> {
    loop {
        match socket.next().await {
            Some(Ok(Message::Text(text))) => return text.as_bytes().to_vec(),
            Some(Ok(Message::Ping(payload))) => {
                socket.send(Message::Pong(payload)).await.unwrap();
            }
            Some(Ok(Message::Pong(_)))
            | Some(Ok(Message::Binary(_)))
            | Some(Ok(Message::Frame(_))) => {}
            other => panic!("expected a text frame, got {other:?}"),
        }
    }
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
    negotiate_with(connection, support::capabilities()).await;
}

async fn negotiate_with(connection: &mut WebSocketConnection, capabilities: CapabilitySet) {
    let response = connection
        .request(RequestEnvelope::new(ClientRequest::Control(
            ControlRequest::DiscoverCapabilities,
        )))
        .await
        .unwrap();
    assert!(matches!(
        response.result,
        Ok(ServerResponse::Control(ControlResponse::Capabilities(_)))
    ));
    let response = connection
        .request(RequestEnvelope::new(ClientRequest::Control(
            ControlRequest::Negotiate {
                client_version: loom_protocol::CURRENT_PROTOCOL_VERSION,
                capabilities,
            },
        )))
        .await
        .unwrap();
    assert!(matches!(
        response.result,
        Ok(ServerResponse::Control(ControlResponse::Negotiated(_)))
    ));
}

async fn create_workspace(connection: &mut WebSocketConnection) -> WorkspaceId {
    match connection
        .request(RequestEnvelope::new(ClientRequest::Workspace(
            WorkspaceRequest::CreateWorkspace {
                name: "remote workspace".to_owned(),
            },
        )))
        .await
        .unwrap()
        .result
        .unwrap()
    {
        ServerResponse::Workspace(WorkspaceResponse::WorkspaceCreated(workspace)) => workspace.id,
        response => panic!("unexpected workspace response: {response:?}"),
    }
}

async fn session(
    connection: &mut WebSocketConnection,
    workspace_id: WorkspaceId,
) -> loom_core::AgentSessionSnapshot {
    match connection
        .request(RequestEnvelope::new(ClientRequest::Workspace(
            WorkspaceRequest::CreateAgentSessionInWorkspace {
                workspace_id,
                name: "remote session".to_owned(),
            },
        )))
        .await
        .unwrap()
        .result
        .unwrap()
    {
        ServerResponse::Session(SessionResponse::AgentSessionCreated(snapshot)) => snapshot,
        response => panic!("unexpected session response: {response:?}"),
    }
}

async fn events(
    connection: &mut WebSocketConnection,
    session_id: AgentSessionId,
    after_sequence: Option<EventSequence>,
) -> Vec<loom_protocol::ServerEventEnvelope> {
    match connection
        .request(RequestEnvelope::new(ClientRequest::Events(
            EventsRequest::GetSessionEvents {
                activity_detail: loom_protocol::ActivityDetail::Full,
                session_id: Some(session_id),
                workspace_id: None,
                after_sequence,
                stream_epoch: None,
            },
        )))
        .await
        .unwrap()
        .result
        .unwrap()
    {
        ServerResponse::Events(EventsResponse::SessionEvents { events, .. })
        | ServerResponse::Events(EventsResponse::SessionEventsSnapshot { events, .. }) => events,
        response => panic!("unexpected event response: {response:?}"),
    }
}

async fn worker_status(connection: &mut WebSocketConnection) -> WorkerNodeStatus {
    match connection
        .request(RequestEnvelope::new(ClientRequest::Control(
            ControlRequest::GetWorkerNodeStatus,
        )))
        .await
        .unwrap()
        .result
        .unwrap()
    {
        ServerResponse::Control(ControlResponse::WorkerNodeStatus(status)) => status,
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

/// A listener that accepts GitHub sign-ins with `allowlist`, and at most
/// `max_anonymous_connections` upgrades that carry no credential.
async fn server_with_sign_in(
    allowlist: &[&str],
    max_anonymous_connections: usize,
) -> loom_server::RunningRemoteServer {
    RemoteServer::new(
        InProcessBackend::new(),
        Arc::new(AuthTokenStore::new()),
        RemoteServerConfig {
            heartbeat_interval: Duration::from_millis(25),
            request_timeout: Duration::from_secs(5),
            github_sign_in: GitHubSignInConfig::new(
                allowlist.iter().map(|login| (*login).to_owned()),
            ),
            max_anonymous_connections,
            ..RemoteServerConfig::local_ephemeral()
        },
    )
    .bind()
    .await
    .unwrap()
}

/// Opens a WebSocket to the test listener without any credential.
async fn upgrade_without_a_credential(
    server: &loom_server::RunningRemoteServer,
) -> Result<WebSocketStream<MaybeTlsStream<TcpStream>>, tokio_tungstenite::tungstenite::Error> {
    let request = server.websocket_url().into_client_request().unwrap();
    connect_async(request).await.map(|(socket, _)| socket)
}

#[tokio::test]
async fn an_upgrade_without_a_credential_is_refused_when_sign_in_is_disabled() {
    let server = RemoteServer::new(
        InProcessBackend::new(),
        Arc::new(AuthTokenStore::new()),
        RemoteServerConfig {
            github_sign_in: GitHubSignInConfig::disabled(),
            ..RemoteServerConfig::local_ephemeral()
        },
    )
    .bind()
    .await
    .unwrap();
    assert!(
        upgrade_without_a_credential(&server).await.is_err(),
        "a worker with GitHub sign-in switched off must refuse a token-less upgrade"
    );
    server.stop().await.unwrap();
}

#[tokio::test]
async fn an_upgrade_without_a_credential_is_a_pre_login_connection_when_sign_in_is_enabled() {
    let server = server_with_sign_in(&["octocat"], 4).await;
    let mut socket = upgrade_without_a_credential(&server)
        .await
        .expect("a worker that allows GitHub sign-in accepts a token-less upgrade");

    // The pre-login connection may discover capabilities and read the worker.
    let discover = encode_request(&RequestEnvelope::new(ClientRequest::Control(
        ControlRequest::DiscoverCapabilities,
    )))
    .unwrap();
    socket
        .send(Message::Text(String::from_utf8(discover).unwrap().into()))
        .await
        .unwrap();
    let response = decode_response(&next_text_frame(&mut socket).await).unwrap();
    assert!(matches!(
        response.result,
        Ok(ServerResponse::Control(ControlResponse::Capabilities(_)))
    ));

    // Anything else is refused with the reason, over the same connection.
    let request = encode_request(&RequestEnvelope::new(ClientRequest::Workspace(
        WorkspaceRequest::ListWorkspaces,
    )))
    .unwrap();
    socket
        .send(Message::Text(String::from_utf8(request).unwrap().into()))
        .await
        .unwrap();
    let response = decode_response(&next_text_frame(&mut socket).await).unwrap();
    let error = response.result.unwrap_err();
    assert_eq!(error.code, ErrorCode::AuthenticationRequired);
    assert!(error.message.contains("not signed in"), "{error}");
    server.stop().await.unwrap();
}

#[tokio::test]
async fn connections_waiting_to_sign_in_are_bounded() {
    let server = server_with_sign_in(&["octocat"], 1).await;
    let _only_slot = upgrade_without_a_credential(&server)
        .await
        .expect("the first connection waiting to sign in is accepted");
    assert!(
        upgrade_without_a_credential(&server).await.is_err(),
        "a second connection waiting to sign in must be refused"
    );
    server.stop().await.unwrap();
}

/// A local stand-in for github.com's device flow and account API, so a
/// sign-in test needs no network. Every round of one sign-in makes three
/// requests: the device authorization, the token exchange, and the account
/// read.
struct GitHubDeviceFlowFixture {
    base: String,
    requests: std::sync::mpsc::Receiver<String>,
    server: std::thread::JoinHandle<()>,
}

/// The GitHub account the fixture reports, and the allowlist that accepts it.
const FIXTURE_GITHUB_USER_ID: u64 = 141030;
const FIXTURE_GITHUB_LOGIN: &str = "octocat";

impl GitHubDeviceFlowFixture {
    fn start(rounds: usize) -> Self {
        use std::{
            io::{Read, Write},
            net::TcpListener,
        };

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let (sender, requests) = std::sync::mpsc::channel();
        let server = std::thread::spawn(move || {
            for _ in 0..rounds * 3 {
                let (mut stream, _) = listener.accept().unwrap();
                let mut request = Vec::new();
                let mut byte = [0_u8; 1];
                while !request.ends_with(b"\r\n\r\n") {
                    if stream.read_exact(&mut byte).is_err() {
                        return;
                    }
                    request.push(byte[0]);
                }
                let request = String::from_utf8_lossy(&request).into_owned();
                let path = request
                    .split_whitespace()
                    .nth(1)
                    .unwrap_or_default()
                    .to_owned();
                let body = match path.as_str() {
                    "/login/device/code" => concat!(
                        r#"{"device_code":"device-code","user_code":"ABCD-1234","#,
                        r#""verification_uri":"https://github.com/login/device","#,
                        r#""expires_in":900,"interval":1}"#
                    )
                    .to_owned(),
                    "/login/oauth/access_token" => {
                        r#"{"access_token":"identity-token","token_type":"bearer"}"#.to_owned()
                    }
                    "/user" => format!(
                        r#"{{"id":{},"login":"{}"}}"#,
                        FIXTURE_GITHUB_USER_ID, FIXTURE_GITHUB_LOGIN
                    ),
                    other => panic!("unexpected fixture request: {other}"),
                };
                let _ = sender.send(path);
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(response.as_bytes());
                let _ = stream.flush();
            }
        });
        Self {
            base: format!("http://{address}"),
            requests,
            server,
        }
    }

    fn config(&self) -> GitHubSignInConfig {
        GitHubSignInConfig::new([FIXTURE_GITHUB_LOGIN.to_owned()])
            .with_device_code_url(format!("{}/login/device/code", self.base))
            .with_access_token_url(format!("{}/login/oauth/access_token", self.base))
            .with_user_api_url(format!("{}/user", self.base))
    }

    /// The paths this fixture served, in order.
    fn served(&self) -> Vec<String> {
        self.requests.try_iter().collect()
    }

    fn join(self) {
        self.server.join().expect("fixture server panicked");
    }
}

/// The capabilities a sign-in test negotiates: the standard fixture plus the
/// identity capabilities these tests exercise, which that fixture predates.
fn sign_in_capabilities() -> CapabilitySet {
    support::capabilities()
        .iter()
        .copied()
        .chain([Capability::GitHubSignIn, Capability::ReadSessionRoster])
        .collect()
}

/// Starts a GitHub sign-in on `connection` and returns the id to poll.
async fn start_github_sign_in(connection: &mut WebSocketConnection) -> String {
    match connection
        .request(RequestEnvelope::new(ClientRequest::Access(
            AccessRequest::StartGitHubSignIn,
        )))
        .await
        .unwrap()
        .result
        .unwrap()
    {
        ServerResponse::Access(AccessResponse::GitHubSignInStarted { login_id, .. }) => login_id,
        response => panic!("unexpected sign-in response: {response:?}"),
    }
}

/// Polls a sign-in until it stops being pending.
async fn await_github_sign_in(
    connection: &mut WebSocketConnection,
    login_id: &str,
) -> loom_core::GitHubSignInStatus {
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    loop {
        let status = match connection
            .request(RequestEnvelope::new(ClientRequest::Access(
                AccessRequest::GetGitHubSignInStatus {
                    login_id: login_id.to_owned(),
                },
            )))
            .await
            .unwrap()
            .result
            .unwrap()
        {
            ServerResponse::Access(AccessResponse::GitHubSignInStatus { status, .. }) => status,
            response => panic!("unexpected status response: {response:?}"),
        };
        if !matches!(status, loom_core::GitHubSignInStatus::Pending) {
            return status;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the sign-in did not settle"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

async fn session_roster(
    connection: &mut WebSocketConnection,
    session_id: AgentSessionId,
) -> Vec<loom_core::RosterEntry> {
    match connection
        .request(RequestEnvelope::new(ClientRequest::Access(
            AccessRequest::GetSessionRoster { session_id },
        )))
        .await
        .unwrap()
        .result
        .unwrap()
    {
        ServerResponse::Access(AccessResponse::SessionRoster { entries, .. }) => entries,
        response => panic!("unexpected roster response: {response:?}"),
    }
}

/// A listener that lets GitHub sign-in through against `fixture`, over
/// `backend`, with `auth` for its tokens.
async fn sign_in_server(
    backend: Arc<InProcessBackend>,
    auth: Arc<AuthTokenStore>,
    fixture: &GitHubDeviceFlowFixture,
) -> loom_server::RunningRemoteServer {
    RemoteServer::new(
        backend,
        auth,
        RemoteServerConfig {
            heartbeat_interval: Duration::from_millis(25),
            request_timeout: Duration::from_secs(5),
            github_sign_in: fixture.config(),
            ..RemoteServerConfig::local_ephemeral()
        },
    )
    .bind()
    .await
    .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unrestricted_operator_sign_in_adopts_every_ownerless_session() {
    let fixture = GitHubDeviceFlowFixture::start(1);
    let auth = Arc::new(AuthTokenStore::new());
    let operator = auth
        .insert("operator-token", AuthorizationScope::all())
        .unwrap();
    let server = sign_in_server(InProcessBackend::new(), Arc::clone(&auth), &fixture).await;
    let mut connection = WebSocketTransport::new(server.websocket_url(), operator.token.clone())
        .connect()
        .await
        .unwrap();
    negotiate_with(&mut connection, sign_in_capabilities()).await;

    // An operator connection creates a workspace and a session without an
    // owner, so nothing needs to be revoked or demoted for the sign-in.
    let workspace_id = create_workspace(&mut connection).await;
    let session = session(&mut connection, workspace_id).await;
    assert!(session_roster(&mut connection, session.id).await.is_empty());

    let login_id = start_github_sign_in(&mut connection).await;
    let loom_core::GitHubSignInStatus::SignedIn { participant } =
        await_github_sign_in(&mut connection, &login_id).await
    else {
        panic!("the sign-in should bind the person");
    };
    assert_eq!(
        participant.id,
        loom_core::github_participant_id(FIXTURE_GITHUB_USER_ID)
    );

    // The person is the owner of the session that had no owner, not merely a
    // person with some access to it. The connection is held to roles after the
    // sign-in, so this roster read also shows the grant arrived before the
    // status was reported.
    let entries = session_roster(&mut connection, session.id).await;
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].participant, participant);
    assert_eq!(entries[0].role, loom_core::SessionRole::Owner);

    // A second client that never signed in sees both effects of the adoption.
    // Its own roster read shows the new owner, and the session feed it reads
    // carries the pushed roster and the attributed history entry that a manual
    // grant of the same role produces, so a co-owner or a second admin window
    // learns of the change without an unrelated refresh.
    let observer = auth
        .insert("observer-token", AuthorizationScope::all())
        .unwrap();
    let mut observer_connection =
        WebSocketTransport::new(server.websocket_url(), observer.token.clone())
            .connect()
            .await
            .unwrap();
    negotiate_with(&mut observer_connection, sign_in_capabilities()).await;
    let observed = session_roster(&mut observer_connection, session.id).await;
    assert_eq!(observed.len(), 1, "{observed:?}");
    assert_eq!(observed[0].participant, participant);
    assert_eq!(observed[0].role, loom_core::SessionRole::Owner);
    let journaled = events(&mut observer_connection, session.id, None).await;
    let pushed = journaled
        .iter()
        .filter_map(|envelope| match &envelope.event {
            ServerEvent::SessionRosterChanged { entries } => Some(entries),
            _ => None,
        })
        .next_back()
        .expect("the adoption publishes the session's roster");
    assert!(
        pushed
            .iter()
            .any(|entry| entry.participant == participant
                && entry.role == loom_core::SessionRole::Owner),
        "{pushed:?}"
    );
    let recorded = journaled
        .iter()
        .filter_map(|envelope| match &envelope.event {
            ServerEvent::Collaboration { entry } => match &entry.kind {
                loom_core::CollaborationEntryKind::AccessChanged {
                    participant: changed,
                    role,
                } => Some((entry.author.clone(), changed.id, *role)),
                _ => None,
            },
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        recorded,
        vec![(
            loom_core::Author::Participant {
                participant: participant.clone(),
                role: loom_core::SessionRole::Owner,
            },
            participant.id,
            Some(loom_core::SessionRole::Owner),
        )]
    );

    // The grant is real authority: an owner-only request now succeeds, where
    // the same connection was held to the role it had before the sign-in.
    let renamed = connection
        .request(RequestEnvelope::new(ClientRequest::Session(
            SessionRequest::RenameAgentSession {
                session_id: session.id,
                name: "adopted".to_owned(),
            },
        )))
        .await
        .unwrap();
    assert!(renamed.result.is_ok(), "{:?}", renamed.result);

    // The operator token still decides what the connection may do: binding a
    // GitHub person must not take away worker-wide repository browsing, which
    // a role-bound participant may not use.
    let cloned = connection
        .request(RequestEnvelope::new(ClientRequest::Repository(
            RepositoryRequest::ListClonedRepositories,
        )))
        .await
        .unwrap();
    assert!(cloned.result.is_ok(), "{:?}", cloned.result);

    assert_eq!(
        fixture.served(),
        vec!["/login/device/code", "/login/oauth/access_token", "/user"]
    );
    fixture.join();
    drop(connection);
    server.stop().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_restricted_operator_sign_in_adopts_nothing() {
    let fixture = GitHubDeviceFlowFixture::start(1);
    let auth = Arc::new(AuthTokenStore::new());
    let operator = auth
        .insert("operator-token", AuthorizationScope::all())
        .unwrap();
    let server = sign_in_server(InProcessBackend::new(), Arc::clone(&auth), &fixture).await;
    let mut operator_connection =
        WebSocketTransport::new(server.websocket_url(), operator.token.clone())
            .connect()
            .await
            .unwrap();
    negotiate_with(&mut operator_connection, sign_in_capabilities()).await;
    let workspace_id = create_workspace(&mut operator_connection).await;
    let session = session(&mut operator_connection, workspace_id).await;

    // A workspace-scoped token reaches only the workspace it names, so a
    // sign-in on it adopts nothing even though it may sign in.
    let scoped = auth
        .insert(
            "workspace-token",
            AuthorizationScope::for_workspaces(
                [workspace_id],
                CapabilitySet::new([Capability::GitHubSignIn]),
            ),
        )
        .unwrap();
    let mut scoped_connection = WebSocketTransport::new(server.websocket_url(), scoped.token)
        .connect()
        .await
        .unwrap();
    negotiate_with(&mut scoped_connection, sign_in_capabilities()).await;
    let login_id = start_github_sign_in(&mut scoped_connection).await;
    let loom_core::GitHubSignInStatus::SignedIn { participant } =
        await_github_sign_in(&mut scoped_connection, &login_id).await
    else {
        panic!("a workspace-scoped token may still sign in");
    };
    assert_eq!(
        participant.id,
        loom_core::github_participant_id(FIXTURE_GITHUB_USER_ID)
    );

    // The session still has no grant, which is what the unrestricted operator
    // connection reads back.
    assert!(
        session_roster(&mut operator_connection, session.id)
            .await
            .is_empty()
    );
    // The signed-in person is held to roles on their own connection: a session
    // the scoped token did not adopt is not readable.
    assert_eq!(
        scoped_connection
            .request(RequestEnvelope::new(ClientRequest::Access(
                AccessRequest::GetSessionRoster {
                    session_id: session.id
                },
            )))
            .await
            .unwrap()
            .result
            .unwrap_err()
            .code,
        ErrorCode::AuthorizationDenied
    );

    fixture.join();
    drop((operator_connection, scoped_connection));
    server.stop().await.unwrap();
}

#[tokio::test]
async fn authorized_client_can_discover_and_revoke_access() {
    let (_backend, auth, token, server) = server().await;
    let transport = WebSocketTransport::new(server.websocket_url(), token.token.clone());
    let mut connection = transport.connect().await.unwrap();
    negotiate(&mut connection).await;
    let discovered = connection
        .request(RequestEnvelope::new(ClientRequest::Control(
            ControlRequest::DiscoverCapabilities,
        )))
        .await
        .unwrap();
    let discovered = match discovered.result {
        Ok(ServerResponse::Control(ControlResponse::Capabilities(result))) => result.capabilities,
        other => panic!("unexpected capability response: {other:?}"),
    };
    assert!(discovered.contains(Capability::ReadSessionFilesystem));
    assert!(discovered.contains(Capability::ReadVcsStatus));
    let workspace_id = create_workspace(&mut connection).await;
    let _ = session(&mut connection, workspace_id).await;
    auth.revoke(&token.token_id).unwrap();
    let response = connection
        .request(RequestEnvelope::new(ClientRequest::Provider(
            ProviderRequest::ListModels,
        )))
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
        .request(RequestEnvelope::new(ClientRequest::Provider(
            ProviderRequest::ConfigureGitHubCopilot {
                access_token: "github-device-flow-token".to_owned(),
            },
        )))
        .await
        .unwrap();
    assert!(matches!(
        response.result,
        Ok(ServerResponse::Provider(
            ProviderResponse::ProviderConfigured
        ))
    ));

    let response = connection
        .request(RequestEnvelope::new(ClientRequest::Provider(
            ProviderRequest::ListProviders,
        )))
        .await
        .unwrap();
    let Ok(ServerResponse::Provider(ProviderResponse::Providers { providers })) = response.result
    else {
        panic!("unexpected provider response: {:?}", response.result);
    };
    assert!(
        providers
            .iter()
            .any(|provider| provider.kind == loom_model::ProviderKind::GitHubCopilot)
    );

    let response = connection
        .request(RequestEnvelope::new(ClientRequest::Provider(
            ProviderRequest::GetGitHubCopilotLoginStatus {
                login_id: "missing-login".to_owned(),
            },
        )))
        .await
        .unwrap();
    assert_eq!(response.result.unwrap_err().code, ErrorCode::NotFound);
    server.stop().await.unwrap();
}

#[tokio::test]
async fn authorized_client_can_configure_api_key_provider_on_remote_worker() {
    let providers = loom_providers::ProviderRegistry::configured(Arc::new(
        loom_providers::InMemoryCredentialStore::default(),
    ))
    .unwrap();
    let backend = InProcessBackend::with_provider_registry(providers);
    let (_backend, _auth, token, server) = server_with_backend(backend).await;
    let mut connection = WebSocketTransport::new(server.websocket_url(), token.token.clone())
        .connect()
        .await
        .unwrap();
    negotiate(&mut connection).await;

    let secret = "remote-provider-key-must-stay-private";
    let response = connection
        .request(RequestEnvelope::new(ClientRequest::Provider(
            ProviderRequest::ConfigureApiKeyProvider {
                provider_id: loom_model::ProviderId::new("openai"),
                api_key: secret.to_owned(),
            },
        )))
        .await
        .unwrap();
    assert!(matches!(
        response.result,
        Ok(ServerResponse::Provider(
            ProviderResponse::ProviderConfigured
        ))
    ));
    assert!(!format!("{response:?}").contains(secret));

    let response = connection
        .request(RequestEnvelope::new(ClientRequest::Provider(
            ProviderRequest::ListProviders,
        )))
        .await
        .unwrap();
    let Ok(ServerResponse::Provider(ProviderResponse::Providers { providers })) = response.result
    else {
        panic!("unexpected provider response: {:?}", response.result);
    };
    let provider = providers
        .iter()
        .find(|provider| provider.id.as_str() == "openai")
        .expect("OpenAI provider should be listed");
    assert_eq!(provider.kind, loom_model::ProviderKind::OpenAi);
    assert_eq!(provider.display_name, "OpenAI");
    assert!(provider.api_key_configurable);
    assert!(provider.credential_id.is_some());
    assert!(!format!("{provider:?}").contains(secret));
    server.stop().await.unwrap();
}

#[tokio::test]
async fn authorized_client_can_configure_deepseek_provider_on_remote_worker() {
    let providers = loom_providers::ProviderRegistry::configured(Arc::new(
        loom_providers::InMemoryCredentialStore::default(),
    ))
    .unwrap();
    let backend = InProcessBackend::with_provider_registry(providers);
    let (_backend, _auth, token, server) = server_with_backend(backend).await;
    let mut connection = WebSocketTransport::new(server.websocket_url(), token.token.clone())
        .connect()
        .await
        .unwrap();
    negotiate(&mut connection).await;

    let secret = "remote-deepseek-key-must-stay-private";
    let response = connection
        .request(RequestEnvelope::new(ClientRequest::Provider(
            ProviderRequest::ConfigureApiKeyProvider {
                provider_id: loom_model::ProviderId::new("deepseek"),
                api_key: secret.to_owned(),
            },
        )))
        .await
        .unwrap();
    assert!(matches!(
        response.result,
        Ok(ServerResponse::Provider(
            ProviderResponse::ProviderConfigured
        ))
    ));
    assert!(!format!("{response:?}").contains(secret));

    let response = connection
        .request(RequestEnvelope::new(ClientRequest::Provider(
            ProviderRequest::ListProviders,
        )))
        .await
        .unwrap();
    let Ok(ServerResponse::Provider(ProviderResponse::Providers { providers })) = response.result
    else {
        panic!("unexpected provider response: {:?}", response.result);
    };
    let provider = providers
        .iter()
        .find(|provider| provider.id.as_str() == "deepseek")
        .expect("DeepSeek provider should be listed");
    assert_eq!(provider.kind, loom_model::ProviderKind::DeepSeek);
    assert_eq!(provider.display_name, "DeepSeek");
    assert!(provider.api_key_configurable);
    assert!(provider.credential_id.is_some());
    assert!(!format!("{provider:?}").contains(secret));
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
        .request(RequestEnvelope::new(ClientRequest::Provider(
            ProviderRequest::ConfigureGitHubCopilot {
                access_token: "github-device-flow-token".to_owned(),
            },
        )))
        .await
        .unwrap();
    assert_eq!(
        response.result.unwrap_err().code,
        ErrorCode::AuthorizationDenied
    );

    for request in [
        ClientRequest::Provider(ProviderRequest::StartGitHubCopilotLogin),
        ClientRequest::Provider(ProviderRequest::GetGitHubCopilotLoginStatus {
            login_id: "unknown".to_owned(),
        }),
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
        .request(RequestEnvelope::new(ClientRequest::Control(
            ControlRequest::Negotiate {
                client_version: loom_protocol::CURRENT_PROTOCOL_VERSION,
                capabilities: support::capabilities(),
            },
        )))
        .result
        .unwrap();
    let allowed_workspace = match setup
        .request(RequestEnvelope::new(ClientRequest::Workspace(
            WorkspaceRequest::CreateWorkspace {
                name: "allowed".to_owned(),
            },
        )))
        .result
        .unwrap()
    {
        ServerResponse::Workspace(WorkspaceResponse::WorkspaceCreated(workspace)) => workspace.id,
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
        .request(RequestEnvelope::new(ClientRequest::Workspace(
            WorkspaceRequest::CreateAgentSessionInWorkspace {
                workspace_id: WorkspaceId::new(),
                name: "denied".to_owned(),
            },
        )))
        .await
        .unwrap();
    assert_eq!(
        denied.result.unwrap_err().code,
        ErrorCode::AuthorizationDenied
    );
    let allowed = session(&mut connection, allowed_workspace).await;
    let denied = connection
        .request(RequestEnvelope::new(ClientRequest::Session(
            SessionRequest::GetAgentSession {
                session_id: allowed.id,
            },
        )))
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
        .request(RequestEnvelope::new(ClientRequest::Control(
            ControlRequest::Negotiate {
                client_version: loom_protocol::CURRENT_PROTOCOL_VERSION,
                capabilities: support::capabilities(),
            },
        )))
        .result
        .unwrap();
    let workspace_id = match setup
        .request(RequestEnvelope::new(ClientRequest::Workspace(
            WorkspaceRequest::CreateWorkspace {
                name: "scoped workspace".to_owned(),
            },
        )))
        .result
        .unwrap()
    {
        ServerResponse::Workspace(WorkspaceResponse::WorkspaceCreated(workspace)) => workspace.id,
        response => panic!("unexpected workspace response: {response:?}"),
    };
    let create_session = || {
        setup
            .request(RequestEnvelope::new(ClientRequest::Workspace(
                WorkspaceRequest::CreateAgentSessionInWorkspace {
                    workspace_id,
                    name: "scoped session".to_owned(),
                },
            )))
            .result
            .unwrap()
    };
    let allowed_session = match create_session() {
        ServerResponse::Session(SessionResponse::AgentSessionCreated(session)) => session,
        response => panic!("unexpected session response: {response:?}"),
    };
    let hidden_session = match create_session() {
        ServerResponse::Session(SessionResponse::AgentSessionCreated(session)) => session,
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
        .request(RequestEnvelope::new(ClientRequest::Workspace(
            WorkspaceRequest::ListWorkspaceSessions {
                workspace_id,
                include_archived: false,
            },
        )))
        .await
        .unwrap()
        .result
        .unwrap();
    let ServerResponse::Session(SessionResponse::AgentSessions { sessions }) = response else {
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
        .request(RequestEnvelope::new(ClientRequest::Workspace(
            WorkspaceRequest::CreateAgentSessionInWorkspace {
                workspace_id,
                name: "unauthorized".to_owned(),
            },
        )))
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
        .request(RequestEnvelope::new(ClientRequest::Session(
            SessionRequest::SetSessionApprovalPolicy {
                session_id: session.id,
                policy: ApprovalPolicy::default(),
                auto_approve_actions: Some(false),
            },
        )))
        .await
        .unwrap()
        .result
        .unwrap();
    let run_id = match first
        .request(RequestEnvelope::new(ClientRequest::Run(
            RunRequest::StartSessionAgentRun {
                session_id: session.id,
                task: "remote durable task".to_owned(),
                model: ModelId::new("deterministic/demo"),
                system_instructions: None,
                repository_instructions: None,
            },
        )))
        .await
        .unwrap()
        .result
        .unwrap()
    {
        ServerResponse::Run(RunResponse::AgentRunStarted(snapshot)) => snapshot.id,
        response => panic!("unexpected run response: {response:?}"),
    };
    drop(first);

    let mut second = transport.connect().await.unwrap();
    negotiate(&mut second).await;
    let mut after = None;
    let mut approvals = 0;
    let mut completed = false;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while !completed && tokio::time::Instant::now() < deadline {
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
                        attempt_id,
                        control_revision,
                        call,
                        ..
                    },
            } = &event.event
                && *event_run == run_id
            {
                approvals += 1;
                second
                    .request(RequestEnvelope::new(ClientRequest::Run(
                        RunRequest::ApproveAgentAction {
                            run_id,
                            attempt_id: *attempt_id,
                            expected_control_revision: *control_revision,
                            tool_call_id: call.id,
                        },
                    )))
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
        .request(RequestEnvelope::new(ClientRequest::Run(
            RunRequest::GetAgentRun { run_id },
        )))
        .await
        .unwrap();
    assert!(matches!(
        run.result.unwrap(),
        ServerResponse::Run(RunResponse::AgentRun(snapshot))
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
    for name in ["first-a", "first-b", "first-c"] {
        connection
            .request(RequestEnvelope::new(ClientRequest::Session(
                SessionRequest::RenameAgentSession {
                    session_id: first.id,
                    name: name.to_owned(),
                },
            )))
            .await
            .unwrap()
            .result
            .unwrap();
    }
    let _fourth = session(&mut connection, workspace_id).await;
    let response = connection
        .request(RequestEnvelope::new(ClientRequest::Events(
            EventsRequest::GetSessionEvents {
                activity_detail: loom_protocol::ActivityDetail::Full,
                session_id: Some(first.id),
                workspace_id: None,
                after_sequence: Some(EventSequence::new(1)),
                stream_epoch: None,
            },
        )))
        .await
        .unwrap();
    let stream_epoch = match response.result.unwrap() {
        ServerResponse::Events(EventsResponse::SessionEventsSnapshot {
            session,
            events,
            oldest_sequence,
            latest_sequence,
            stream_epoch: Some(stream_epoch),
        }) => {
            assert_eq!(session.id, first.id);
            assert_eq!(events.len(), 2);
            assert_eq!(oldest_sequence.value(), 5);
            assert_eq!(latest_sequence.value(), 6);
            stream_epoch
        }
        response => panic!("expected snapshot fallback, got {response:?}"),
    };
    let resumed = connection
        .request(RequestEnvelope::new(ClientRequest::Events(
            EventsRequest::GetSessionEvents {
                activity_detail: loom_protocol::ActivityDetail::Full,
                session_id: Some(first.id),
                workspace_id: None,
                after_sequence: Some(EventSequence::new(6)),
                stream_epoch: Some(stream_epoch.clone()),
            },
        )))
        .await
        .unwrap();
    assert!(matches!(
        resumed.result.unwrap(),
        ServerResponse::Events(EventsResponse::SessionEvents{
            stream_epoch: Some(epoch),
            ..
        }) if epoch == stream_epoch
    ));
    server.stop().await.unwrap();
}

#[tokio::test]
async fn session_cursor_before_creation_ignores_other_sessions_events() {
    let (_backend, _auth, token, server) = server().await;
    let transport = WebSocketTransport::new(server.websocket_url(), token.token);
    let mut connection = transport.connect().await.unwrap();
    negotiate(&mut connection).await;
    let workspace_id = create_workspace(&mut connection).await;
    let _earlier_session = session(&mut connection, workspace_id).await;
    let target_session = session(&mut connection, workspace_id).await;
    let response = connection
        .request(RequestEnvelope::new(ClientRequest::Events(
            EventsRequest::GetSessionEvents {
                activity_detail: loom_protocol::ActivityDetail::Full,
                session_id: Some(target_session.id),
                workspace_id: None,
                after_sequence: Some(EventSequence::default()),
                stream_epoch: None,
            },
        )))
        .await
        .unwrap();
    let ServerResponse::Events(EventsResponse::SessionEvents { events, .. }) =
        response.result.unwrap()
    else {
        panic!("an unrelated session's prior events must not stale this cursor");
    };
    assert!(events.iter().any(|event| {
        event.session_id == target_session.id
            && matches!(
                event.event,
                loom_protocol::ServerEvent::AgentSessionCreated { .. }
            )
    }));
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
        ClientRequest::Workspace(WorkspaceRequest::CreateAgentSessionInWorkspace {
            workspace_id,
            name: "idempotent".to_owned(),
        }),
    );
    let first = connection.request(request.clone()).await.unwrap();
    drop(connection);
    let mut reconnected = transport.connect().await.unwrap();
    negotiate(&mut reconnected).await;
    let second = reconnected.request(request).await.unwrap();
    assert_eq!(first, second);
    let ServerResponse::Session(SessionResponse::AgentSessionCreated(snapshot)) =
        first.result.unwrap()
    else {
        panic!("unexpected idempotent response");
    };
    let events = events(&mut reconnected, snapshot.id, None).await;
    assert_eq!(events.len(), 1);
    server.stop().await.unwrap();
}

#[tokio::test]
async fn tls_end_to_end_handshake_succeeds_with_the_ca_and_fails_without_it() {
    const { assert!(WEBSOCKET_TLS_SUPPORTED) };
    let backend = InProcessBackend::new();
    let auth = Arc::new(AuthTokenStore::new());
    let token = auth
        .insert("tls-test-token", AuthorizationScope::all())
        .unwrap();
    let server = RemoteServer::new(
        backend,
        auth,
        RemoteServerConfig {
            tls: Some(ServerTlsConfig::from_pem(TEST_CERT_PEM, TEST_KEY_PEM)),
            ..RemoteServerConfig::local_ephemeral()
        },
    )
    .bind()
    .await
    .unwrap();
    assert!(server.tls_enabled());
    assert!(
        server.websocket_url().starts_with("wss://127.0.0.1:"),
        "{}",
        server.websocket_url()
    );
    assert!(server.health_url().starts_with("https://"));

    // The same endpoint is rejected when the CA that issued its certificate is
    // not trusted, with an actionable TLS message rather than a DNS hint.
    let untrusted = match WebSocketTransport::new(server.websocket_url(), token.token.clone())
        .connect()
        .await
    {
        Ok(_) => panic!("a worker certificate from an untrusted CA was accepted"),
        Err(error) => error,
    };
    assert!(untrusted.message.contains("TLS handshake"), "{untrusted}");
    assert!(untrusted.message.contains("certificate"), "{untrusted}");
    assert!(
        !untrusted
            .message
            .contains("check the URL and network access"),
        "{untrusted}"
    );

    // Trusting the issuing CA in addition to the OS roots completes the
    // handshake and a real protocol request.
    let mut connection = WebSocketTransport::new(server.websocket_url(), token.token.clone())
        .with_ca_certificate_pem(TEST_CA_PEM)
        .unwrap()
        .connect()
        .await
        .unwrap();
    negotiate(&mut connection).await;
    let workspace_id = create_workspace(&mut connection).await;
    let session = session(&mut connection, workspace_id).await;
    assert_eq!(session.workspace_id, workspace_id);

    drop(connection);
    server.stop().await.unwrap();
}

#[tokio::test]
async fn plaintext_remote_endpoints_require_the_insecure_opt_in() {
    let error = match WebSocketTransport::new("ws://worker.example:8765/ws", "remote-test-token")
        .connect()
        .await
    {
        Ok(_) => panic!("a plaintext remote endpoint was accepted without an opt-in"),
        Err(error) => error,
    };
    assert_eq!(error.code, ErrorCode::InvalidRequest);
    assert!(error.message.contains("--allow-insecure-remote"), "{error}");
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
async fn an_oversized_response_fails_only_its_own_request() {
    let limit = 256;
    let (_backend, _auth, token, server) = server().await;
    let mut connection = WebSocketTransport::new(server.websocket_url(), token.token)
        .with_max_frame_bytes(limit)
        .unwrap()
        .connect()
        .await
        .unwrap();

    let request =
        RequestEnvelope::new(ClientRequest::Control(ControlRequest::DiscoverCapabilities));
    let request_id = request.request_id;
    let error = connection
        .request(request)
        .await
        .expect_err("a response above the frame limit must fail");
    assert_eq!(error.code, ErrorCode::PayloadTooLarge);
    assert!(!error.retryable);
    // The failure names the awaited request, the received size, and the limit,
    // and it comes from the response path rather than from send time.
    assert!(error.message.starts_with("remote response of "), "{error}");
    assert!(
        error.message.contains(&format!("for request {request_id}")),
        "{error}"
    );
    assert!(
        error
            .message
            .contains(&format!("frame size limit of {limit} bytes")),
        "{error}"
    );
    server.stop().await.unwrap();
}

#[tokio::test]
async fn an_oversized_request_is_rejected_before_it_is_sent() {
    let limit = 64;
    let (_backend, _auth, token, server) = server().await;
    let mut connection = WebSocketTransport::new(server.websocket_url(), token.token)
        .with_max_frame_bytes(limit)
        .unwrap()
        .connect()
        .await
        .unwrap();

    let request =
        RequestEnvelope::new(ClientRequest::Control(ControlRequest::DiscoverCapabilities));
    let request_id = request.request_id;
    let encoded = encode_request(&request).unwrap();
    assert!(encoded.len() > limit);
    let error = connection
        .request(request)
        .await
        .expect_err("a request above the frame limit must fail");
    assert_eq!(error.code, ErrorCode::PayloadTooLarge);
    assert_eq!(
        error.message,
        format!(
            "remote request of {} bytes for request {request_id} is above the configured frame \
             size limit of {limit} bytes",
            encoded.len()
        )
    );
    server.stop().await.unwrap();
}

#[tokio::test]
async fn oversized_inbound_frames_are_correlated_or_closed_with_the_limit() {
    let limit = 2048;
    let (_backend, _auth, token, server) = server_with_frame_limit(limit).await;

    // A frame that still decodes is answered under the request id it names.
    let mut socket = raw_socket(&server, &token.token).await;
    let request = padding_request(limit * 2);
    let encoded = encode_request(&request).unwrap();
    assert!(encoded.len() > limit);
    socket
        .send(Message::Text(
            String::from_utf8(encoded.clone()).unwrap().into(),
        ))
        .await
        .unwrap();
    let response = decode_response(&next_text_frame(&mut socket).await).unwrap();
    assert_eq!(response.request_id, request.request_id);
    let error = response.result.unwrap_err();
    assert_eq!(error.code, ErrorCode::PayloadTooLarge);
    assert!(
        error
            .message
            .contains(&format!("of {} bytes", encoded.len())),
        "{error}"
    );
    assert!(
        error.message.contains(&format!("limit of {limit} bytes")),
        "{error}"
    );
    assert!(
        error
            .message
            .contains(&format!("for request {}", request.request_id)),
        "{error}"
    );

    // A frame that names no request cannot be answered, so the connection
    // closes with a reason carrying the same explanation. The oversized bytes
    // must be valid UTF-8, so they are text that is not a request.
    let mut socket = raw_socket(&server, &token.token).await;
    let oversized = "x".repeat(limit * 2);
    socket
        .send(Message::Text(oversized.clone().into()))
        .await
        .unwrap();
    let close = loop {
        match socket.next().await {
            Some(Ok(Message::Close(Some(frame)))) => break frame,
            Some(Ok(Message::Ping(payload))) => {
                socket.send(Message::Pong(payload)).await.unwrap();
            }
            Some(Ok(Message::Pong(_)))
            | Some(Ok(Message::Binary(_)))
            | Some(Ok(Message::Frame(_))) => {}
            other => panic!("expected a close frame, got {other:?}"),
        }
    };
    assert_eq!(u16::from(close.code), 1009);
    let reason = close.reason.as_str();
    assert!(reason.contains(&oversized.len().to_string()), "{reason}");
    assert!(reason.contains(&limit.to_string()), "{reason}");
    server.stop().await.unwrap();
}

#[tokio::test]
async fn a_configured_frame_limit_is_honoured_end_to_end() {
    let limit = 2048;
    let (_backend, _auth, token, server) = server_with_frame_limit(limit).await;
    let mut connection = WebSocketTransport::new(server.websocket_url(), token.token)
        .with_max_frame_bytes(limit)
        .unwrap()
        .connect()
        .await
        .unwrap();

    // Frames under the configured limit still negotiate and reach the request
    // handler on both sides.
    negotiate(&mut connection).await;
    let under_limit = padding_request(limit / 4);
    assert!(encode_request(&under_limit).unwrap().len() < limit);
    let response = connection.request(under_limit).await.unwrap();
    assert_eq!(
        response.result.unwrap_err().code,
        ErrorCode::NotFound,
        "a frame under the limit must be dispatched"
    );

    // A frame above the limit is rejected rather than dispatched, and the
    // rejection names the request, the size, and the limit.
    let over_limit = padding_request(limit * 2);
    let request_id = over_limit.request_id;
    let encoded = encode_request(&over_limit).unwrap();
    assert!(encoded.len() > limit);
    let error = connection
        .request(over_limit)
        .await
        .expect_err("a frame above the configured limit must be rejected");
    assert_eq!(error.code, ErrorCode::PayloadTooLarge);
    assert_eq!(
        error.message,
        format!(
            "remote request of {} bytes for request {request_id} is above the configured frame \
             size limit of {limit} bytes",
            encoded.len()
        )
    );
    server.stop().await.unwrap();
}

#[tokio::test]
async fn a_batch_above_the_frame_limit_is_delivered_in_followable_pages() {
    // One value bounds both sides, as an operator configures with the flag or
    // the environment variable, so the server has to bound the events batch it
    // sends to a frame this client accepts instead of sending one its own rule
    // would drop.
    let limit = 2048;
    let (_backend, _auth, token, server) = server_with_frame_limit(limit).await;
    let mut connection = WebSocketTransport::new(server.websocket_url(), token.token)
        .with_max_frame_bytes(limit)
        .unwrap()
        .connect()
        .await
        .unwrap();
    negotiate(&mut connection).await;
    let workspace_id = create_workspace(&mut connection).await;
    let session = session(&mut connection, workspace_id).await;

    // Every rename journals one event, so the session's backlog grows past the
    // frame limit.
    let renames = 40;
    for index in 0..renames {
        let renamed = connection
            .request(RequestEnvelope::new(ClientRequest::Session(
                SessionRequest::RenameAgentSession {
                    session_id: session.id,
                    name: format!("renamed-{index:03}"),
                },
            )))
            .await
            .unwrap();
        assert!(renamed.result.is_ok(), "{:?}", renamed.result);
    }

    // The client follows the pages the response names until it reaches the
    // latest event. Every frame it accepts is below its own limit, so a response
    // that crossed the limit would fail this request instead.
    let mut cursor = None;
    let mut pages = 0;
    let mut sequences = Vec::new();
    let mut renamed = 0;
    loop {
        let response = connection
            .request(RequestEnvelope::new(ClientRequest::Events(
                EventsRequest::GetSessionEvents {
                    activity_detail: loom_protocol::ActivityDetail::Full,
                    session_id: Some(session.id),
                    workspace_id: None,
                    after_sequence: cursor,
                    stream_epoch: None,
                },
            )))
            .await
            .unwrap();
        let ServerResponse::Events(EventsResponse::SessionEvents {
            events,
            next_after_sequence,
            ..
        }) = response.result.unwrap()
        else {
            panic!("expected a bounded events page");
        };
        assert!(
            !events.is_empty(),
            "every page must make progress towards the latest event"
        );
        renamed += events
            .iter()
            .filter(|event| {
                event.session_id == session.id
                    && matches!(event.event, ServerEvent::AgentSessionRenamed { .. })
            })
            .count();
        sequences.extend(events.iter().map(|event| event.sequence));
        pages += 1;
        match next_after_sequence {
            Some(next) => cursor = Some(next),
            None => break,
        }
    }
    assert!(
        pages > 1,
        "a backlog above the frame limit must arrive in more than one page"
    );
    assert_eq!(renamed, renames);
    assert!(
        sequences.windows(2).all(|pair| pair[0] < pair[1]),
        "pages must carry events in sequence order"
    );

    // The last page reached the latest journaled event, so the same cursor is
    // answered with no further events.
    let latest = *sequences.last().expect("the batch carried events");
    let drained = connection
        .request(RequestEnvelope::new(ClientRequest::Events(
            EventsRequest::GetSessionEvents {
                activity_detail: loom_protocol::ActivityDetail::Full,
                session_id: Some(session.id),
                workspace_id: None,
                after_sequence: Some(latest),
                stream_epoch: None,
            },
        )))
        .await
        .unwrap();
    assert!(matches!(
        drained.result.unwrap(),
        ServerResponse::Events(EventsResponse::SessionEvents {
            events,
            next_after_sequence: None,
            ..
        }) if events.is_empty()
    ));
    server.stop().await.unwrap();
}

#[tokio::test]
async fn an_oversized_frame_that_is_not_the_awaited_response_does_not_fail_the_request() {
    let limit = 512;
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    // The scripted peer sends an over-limit frame that belongs to another
    // request before answering the awaited request.
    let scripted = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut socket = accept_async(stream).await.unwrap();
        let request = loop {
            match socket.next().await {
                Some(Ok(Message::Text(text))) => break decode_request(text.as_bytes()).unwrap(),
                Some(Ok(Message::Ping(payload))) => {
                    socket.send(Message::Pong(payload)).await.unwrap();
                }
                Some(Ok(Message::Pong(_)))
                | Some(Ok(Message::Binary(_)))
                | Some(Ok(Message::Frame(_))) => {}
                other => panic!("expected a request frame, got {other:?}"),
            }
        };
        let foreign = encode_response(&ResponseEnvelope::failure(
            RequestId::new(),
            LoomError::invalid_request("x".repeat(limit * 2)),
        ))
        .unwrap();
        socket
            .send(Message::Text(String::from_utf8(foreign).unwrap().into()))
            .await
            .unwrap();
        let awaited = encode_response(&ResponseEnvelope::failure(
            request.request_id,
            LoomError::not_found("agent session", request.request_id),
        ))
        .unwrap();
        socket
            .send(Message::Text(String::from_utf8(awaited).unwrap().into()))
            .await
            .unwrap();
    });

    let mut connection = WebSocketTransport::new(format!("ws://{address}/ws"), "token")
        .with_max_frame_bytes(limit)
        .unwrap()
        .connect()
        .await
        .unwrap();
    let request =
        RequestEnvelope::new(ClientRequest::Control(ControlRequest::DiscoverCapabilities));
    let request_id = request.request_id;
    let response = connection.request(request).await.unwrap_or_else(|error| {
        panic!("an over-limit frame for another request must not fail this one: {error}")
    });
    assert_eq!(response.request_id, request_id);
    assert_eq!(response.result.unwrap_err().code, ErrorCode::NotFound);
    scripted.await.unwrap();
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
        .request(RequestEnvelope::new(ClientRequest::Terminal(
            TerminalRequest::OpenSessionTerminal {
                session_id: session.id,
                command: "sh".to_owned(),
                args: vec!["-c".to_owned(), "sleep 0.05; printf done".to_owned()],
                cwd: None,
            },
        )))
        .await
        .unwrap()
        .result
        .unwrap()
    {
        ServerResponse::Terminal(TerminalResponse::TerminalOpened(snapshot)) => snapshot,
        response => panic!("unexpected terminal response: {response:?}"),
    };
    drop(first);

    let mut second = transport.connect().await.unwrap();
    negotiate(&mut second).await;
    let mut exited = false;
    for _ in 0..100 {
        let response = second
            .request(RequestEnvelope::new(ClientRequest::Terminal(
                TerminalRequest::GetSessionTerminalEvents {
                    session_id: session.id,
                    terminal_id: terminal.id,
                    after_sequence: None,
                },
            )))
            .await
            .unwrap();
        let ServerResponse::Terminal(TerminalResponse::TerminalEvents { events }) =
            response.result.unwrap()
        else {
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

/// A terminal command that never reads its input, so a large write to it cannot
/// complete before the frames pipelined behind it are received.
const BLOCKING_TERMINAL_COMMAND: &str = "sleep 5";

#[tokio::test]
async fn a_pipelining_client_is_refused_at_the_in_flight_limit_and_keeps_control_requests() {
    // One in-flight request per class, so the first frame holds the
    // connection's only ordinary permit for as long as it runs.
    let (_backend, _auth, token, server) =
        server_with_limits(InProcessBackend::new(), DEFAULT_MAX_FRAME_BYTES, 1, 1).await;
    let mut setup = WebSocketTransport::new(server.websocket_url(), token.token.clone())
        .connect()
        .await
        .unwrap();
    negotiate(&mut setup).await;
    let workspace_id = create_workspace(&mut setup).await;
    let session = session(&mut setup, workspace_id).await;
    let terminal = match setup
        .request(RequestEnvelope::new(ClientRequest::Terminal(
            TerminalRequest::OpenSessionTerminal {
                session_id: session.id,
                command: "sh".to_owned(),
                args: vec!["-c".to_owned(), BLOCKING_TERMINAL_COMMAND.to_owned()],
                cwd: None,
            },
        )))
        .await
        .unwrap()
        .result
        .unwrap()
    {
        ServerResponse::Terminal(TerminalResponse::TerminalOpened(snapshot)) => snapshot,
        response => panic!("unexpected terminal response: {response:?}"),
    };

    // A write this large cannot complete until the terminal exits, so the
    // first frame holds the connection's only ordinary permit while the two
    // frames behind it are received.
    let blocking = RequestEnvelope::new(ClientRequest::Terminal(
        TerminalRequest::WriteSessionTerminalInput {
            session_id: session.id,
            terminal_id: terminal.id,
            input: "p".repeat(1 << 20),
        },
    ));
    let refused = RequestEnvelope::new(ClientRequest::Workspace(
        WorkspaceRequest::CreateWorkspace {
            name: "pipelined".to_owned(),
        },
    ));
    let control =
        RequestEnvelope::new(ClientRequest::Control(ControlRequest::DiscoverCapabilities));
    let mut pipelining = raw_socket(&server, &token.token).await;
    // The raw socket negotiates first, like any client, and waits for the
    // answer, so its control permit is free before the pipeline is admitted.
    let handshake = RequestEnvelope::new(ClientRequest::Control(ControlRequest::Negotiate {
        client_version: loom_protocol::CURRENT_PROTOCOL_VERSION,
        capabilities: support::capabilities(),
    }));
    pipelining
        .send(Message::Text(
            String::from_utf8(encode_request(&handshake).unwrap())
                .unwrap()
                .into(),
        ))
        .await
        .unwrap();
    let answer = tokio::time::timeout(Duration::from_secs(10), next_text_frame(&mut pipelining))
        .await
        .expect("the negotiating frame must be answered");
    let answer = decode_response(&answer).unwrap();
    assert_eq!(answer.request_id, handshake.request_id);
    assert!(answer.result.is_ok(), "{:?}", answer.result);

    for frame in [&blocking, &refused, &control] {
        pipelining
            .send(Message::Text(
                String::from_utf8(encode_request(frame).unwrap())
                    .unwrap()
                    .into(),
            ))
            .await
            .unwrap();
    }

    // A second connection is unaffected while the first is at its limit.
    let mut other = WebSocketTransport::new(server.websocket_url(), token.token.clone())
        .connect()
        .await
        .unwrap();
    negotiate(&mut other).await;
    let read = other
        .request(RequestEnvelope::new(ClientRequest::Session(
            SessionRequest::GetAgentSession {
                session_id: session.id,
            },
        )))
        .await
        .unwrap();
    assert!(read.result.is_ok(), "{:?}", read.result);

    // Every pipelined frame is answered: the refusal and the control request
    // while the first request is still blocked, and the first request once its
    // terminal has drained the write.
    let mut order = Vec::new();
    let mut answers = HashMap::new();
    while answers.len() < 3 {
        let frame = tokio::time::timeout(Duration::from_secs(30), next_text_frame(&mut pipelining))
            .await
            .expect("every pipelined frame must be answered");
        let response = decode_response(&frame).unwrap();
        order.push(response.request_id);
        answers.insert(response.request_id, response.result);
    }
    // The blocked request is answered last, which is what held the permit while
    // the frames behind it were received.
    assert_eq!(order.last(), Some(&blocking.request_id), "{order:?}");

    let refusal = answers
        .remove(&refused.request_id)
        .expect("the pipelined ordinary request must be answered")
        .expect_err("a request past the in-flight limit must be refused");
    assert_eq!(refusal.code, ErrorCode::Backpressure);
    assert!(refusal.retryable);
    assert_eq!(
        refusal.message,
        format!(
            "request {} was not started: this connection is at its configured limit of 1 \
             in-flight requests (max_in_flight_requests); wait for an outstanding response \
             before sending another",
            refused.request_id
        )
    );

    let capabilities = answers
        .remove(&control.request_id)
        .expect("the control request must be answered")
        .expect("pipelined ordinary requests must not starve a control request");
    assert!(matches!(
        capabilities,
        ServerResponse::Control(ControlResponse::Capabilities(_))
    ));

    // The first request was started, not refused, so it really did hold the
    // permit while the other two frames arrived.
    let blocked = answers
        .remove(&blocking.request_id)
        .expect("the blocking request must be answered");
    if let Err(error) = &blocked {
        assert_ne!(
            error.code,
            ErrorCode::Backpressure,
            "the blocking request was refused: {error}"
        );
    }

    drop(pipelining);
    drop(other);
    server.stop().await.unwrap();
}
