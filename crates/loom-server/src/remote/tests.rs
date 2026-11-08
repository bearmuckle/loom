use super::{
    CLOSE_MESSAGE_TOO_BIG, ConnectionControl, ConnectionLimits, DEFAULT_MAX_ANONYMOUS_CONNECTIONS,
    DEFAULT_MAX_FRAME_BYTES, DEFAULT_MAX_IN_FLIGHT_CONTROL_REQUESTS,
    DEFAULT_MAX_IN_FLIGHT_REQUESTS, InFlightLimits, MAX_FRAME_BYTES_ENV, MAX_FRAME_BYTES_FLAG,
    OversizedFrameAction, RemoteServer, RemoteServerConfig, RemoteState, RequestClass,
    ServerTlsConfig, WEBSOCKET_TLS_SUPPORTED, WebSocketTransport, abandoned_request_error,
    auth_error_response, bearer_token, client_tls_config, execute_request_with, frame_request_id,
    frame_too_large_close_reason, frame_too_large_error, health_handler, insecure_remote_host,
    is_control_or_interrupt_request, oversized_inbound_frame_action, process_text, request_auth,
    resolve_max_frame_bytes, try_send_response, websocket_connect_error,
};
use axum::{
    extract::{State, ws::Message},
    http::{
        HeaderMap, HeaderValue,
        header::{AUTHORIZATION, SEC_WEBSOCKET_PROTOCOL},
    },
    response::IntoResponse,
};
use loom_core::{ErrorCode, ProtocolVersion, RequestId};
use loom_protocol::{
    ClientFrame, ClientRequest, ControlRequest, ControlResponse, RequestEnvelope, ResponseEnvelope,
    RunRequest, ServerResponse, WorkspaceRequest, decode_response, encode_client_frame,
    encode_request, encode_response,
};
use std::{
    collections::HashMap,
    io,
    net::{IpAddr, Ipv6Addr, SocketAddr},
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio::sync::Semaphore;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
    sync::mpsc,
};
use tokio_tungstenite::tungstenite::Error as TungsteniteError;

use super::super::AuthorizationScope;
use super::{AuthTokenStore, GitHubSignInConfig, InProcessBackend};
use tokio_tungstenite::{
    connect_async,
    tungstenite::{Error as TungsteniteClientError, error::UrlError, http::StatusCode},
};

/// Test-only TLS fixtures: a CA and a leaf for `127.0.0.1`/`localhost` that
/// are valid until 2126. They exist only so the end-to-end TLS test can run
/// without reaching the network or a real certificate authority.
const TEST_CERT_PEM: &[u8] = include_bytes!("../../tests/fixtures/tls/server-cert.pem");
const TEST_KEY_PEM: &[u8] = include_bytes!("../../tests/fixtures/tls/server-key.pem");
const TEST_CA_PEM: &[u8] = include_bytes!("../../tests/fixtures/tls/ca-cert.pem");

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("tls")
        .join(name)
}

fn test_tls() -> ServerTlsConfig {
    ServerTlsConfig::from_pem(TEST_CERT_PEM, TEST_KEY_PEM)
}

/// The per-connection bounds a frame-loop test applies: `max_frame_bytes` as
/// the inbound frame limit and the default in-flight request limits.
fn test_limits(max_frame_bytes: usize) -> ConnectionLimits {
    ConnectionLimits {
        max_frame_bytes,
        request_timeout: Duration::from_secs(1),
        in_flight: InFlightLimits::new(
            DEFAULT_MAX_IN_FLIGHT_REQUESTS,
            DEFAULT_MAX_IN_FLIGHT_CONTROL_REQUESTS,
        ),
    }
}

#[tokio::test]
async fn remote_websocket_rejects_unauthenticated_upgrade_and_protocol_9() {
    let backend = InProcessBackend::new();
    let auth = std::sync::Arc::new(AuthTokenStore::new());
    let token = auth.issue(AuthorizationScope::all()).unwrap();
    // Sign-in is off, so the bearer token is the only way in.
    let server = RemoteServer::new(
        backend,
        auth,
        RemoteServerConfig {
            github_sign_in: GitHubSignInConfig::disabled(),
            ..RemoteServerConfig::local_ephemeral()
        },
    )
    .bind()
    .await
    .unwrap();

    match connect_async(server.websocket_url()).await {
        Err(TungsteniteClientError::Http(response)) => {
            assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        }
        result => panic!("unauthenticated WebSocket upgrade was not rejected: {result:?}"),
    }

    let mut connection = WebSocketTransport::new(server.websocket_url(), token.token)
        .connect()
        .await
        .unwrap();
    let protocol_9 = connection
        .request(RequestEnvelope::with_version(
            ProtocolVersion::new(9, 0),
            ClientRequest::Control(ControlRequest::DiscoverCapabilities),
        ))
        .await
        .unwrap();
    assert_eq!(
        protocol_9.result.unwrap_err().code,
        ErrorCode::UnsupportedProtocol
    );

    let protocol_10 = connection
        .request(RequestEnvelope::new(ClientRequest::Control(
            ControlRequest::DiscoverCapabilities,
        )))
        .await
        .unwrap();
    assert!(matches!(
        protocol_10.result,
        Ok(ServerResponse::Control(ControlResponse::Capabilities(_)))
    ));

    drop(connection);
    server.stop().await.unwrap();
}

#[tokio::test]
async fn a_listener_attaches_the_token_store_it_authenticates_with() {
    use loom_core::{ParticipantId, ParticipantIdentity};

    let backend = InProcessBackend::new();
    let auth = Arc::new(AuthTokenStore::new());
    let server = RemoteServer::new(
        backend.clone(),
        auth.clone(),
        RemoteServerConfig::local_ephemeral(),
    )
    .bind()
    .await
    .unwrap();

    // The store the listener authenticates connections with is the one a
    // sign-in mints its credential from, so a later connection is accepted
    // by the same listener.
    let person = ParticipantIdentity {
        id: ParticipantId::new(),
        display_name: "octocat".to_owned(),
    };
    let credential = backend
        .issue_person_token(&person)
        .unwrap()
        .expect("a bound listener issues a credential for a person")
        .token;
    let session = auth
        .authenticate(&credential)
        .expect("the listener store authenticates the credential it issued");
    assert_eq!(session.participant(), Some(&person));

    server.stop().await.unwrap();
}

#[tokio::test]
async fn revoking_access_closes_only_the_revoked_persons_sockets() {
    use loom_core::{ParticipantId, ParticipantIdentity};
    use loom_protocol::{AccessRequest, SessionRequest};

    let backend = InProcessBackend::new();
    let auth = Arc::new(AuthTokenStore::new());
    let person = |name: &str| ParticipantIdentity {
        id: ParticipantId::new(),
        display_name: name.to_owned(),
    };
    let owner = person("Owner");
    let guest = person("Guest");
    let owner_token = auth
        .issue_for_participant(AuthorizationScope::all(), owner.clone())
        .unwrap();
    let guest_token = auth
        .issue_for_participant(AuthorizationScope::all(), guest.clone())
        .unwrap();
    backend
        .register_participants(&[owner.clone(), guest.clone()])
        .unwrap();
    let server = RemoteServer::new(
        backend.clone(),
        auth.clone(),
        RemoteServerConfig::local_ephemeral(),
    )
    .bind()
    .await
    .unwrap();

    let negotiate = || {
        RequestEnvelope::new(ClientRequest::Control(ControlRequest::Negotiate {
            client_version: loom_core::CURRENT_PROTOCOL_VERSION,
            capabilities: backend.supported_capabilities.clone(),
        }))
    };
    let mut owner_socket = WebSocketTransport::new(server.websocket_url(), owner_token.token)
        .connect()
        .await
        .unwrap();
    owner_socket.request(negotiate()).await.unwrap();
    let workspace = match owner_socket
        .request(RequestEnvelope::new(ClientRequest::Workspace(
            WorkspaceRequest::CreateWorkspace {
                name: "Shared".to_owned(),
            },
        )))
        .await
        .unwrap()
        .result
        .unwrap()
    {
        ServerResponse::Workspace(loom_protocol::WorkspaceResponse::WorkspaceCreated(
            workspace,
        )) => workspace,
        response => panic!("unexpected workspace response: {response:?}"),
    };
    let session_id = match owner_socket
        .request(RequestEnvelope::new(ClientRequest::Workspace(
            WorkspaceRequest::CreateAgentSessionInWorkspace {
                workspace_id: workspace.id,
                name: "Pairing".to_owned(),
            },
        )))
        .await
        .unwrap()
        .result
        .unwrap()
    {
        ServerResponse::Session(loom_protocol::SessionResponse::AgentSessionCreated(session)) => {
            session.id
        }
        response => panic!("unexpected session response: {response:?}"),
    };
    let grant = owner_socket
        .request(RequestEnvelope::new(ClientRequest::Access(
            AccessRequest::GrantSessionAccess {
                session_id,
                participant_id: guest.id,
            },
        )))
        .await
        .unwrap();
    assert!(grant.result.is_ok());

    let mut guest_socket = WebSocketTransport::new(server.websocket_url(), guest_token.token)
        .connect()
        .await
        .unwrap();
    guest_socket.request(negotiate()).await.unwrap();
    let read = || {
        RequestEnvelope::new(ClientRequest::Session(SessionRequest::GetAgentSession {
            session_id,
        }))
    };
    assert!(guest_socket.request(read()).await.unwrap().result.is_ok());

    let revoked = owner_socket
        .request(RequestEnvelope::new(ClientRequest::Access(
            AccessRequest::RevokeSessionAccess {
                session_id,
                participant_id: guest.id,
            },
        )))
        .await
        .unwrap();
    assert!(revoked.result.is_ok());

    // The revoked person's socket, and every subscription on it, ends.
    let closed = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match guest_socket.request(read()).await {
                Err(_) => break,
                Ok(response) if response.result.is_err() => {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
                Ok(response) => panic!("revoked read succeeded: {response:?}"),
            }
        }
    })
    .await;
    assert!(closed.is_ok(), "the revoked socket was not closed");
    // Other people stay connected.
    assert!(owner_socket.request(read()).await.unwrap().result.is_ok());

    drop((owner_socket, guest_socket));
    server.stop().await.unwrap();
}

#[test]
fn request_auth_prefers_a_valid_bearer_header_over_a_subprotocol() {
    let mut headers = HeaderMap::new();
    headers.insert(
        SEC_WEBSOCKET_PROTOCOL,
        HeaderValue::from_static("loom.bearer.subprotocol-secret"),
    );
    assert_eq!(
        request_auth(&headers).unwrap(),
        ("subprotocol-secret", Some("loom.bearer.subprotocol-secret"))
    );
    headers.insert(
        AUTHORIZATION,
        HeaderValue::from_static("Bearer header-secret"),
    );
    assert_eq!(bearer_token(&headers).unwrap(), "header-secret");
    assert_eq!(request_auth(&headers).unwrap(), ("header-secret", None));
}

#[test]
fn request_auth_rejects_missing_empty_or_malformed_credentials() {
    let headers = HeaderMap::new();
    assert_eq!(
        request_auth(&headers).unwrap_err().code,
        ErrorCode::AuthenticationRequired
    );
    assert_eq!(
        bearer_token(&headers).unwrap_err().code,
        ErrorCode::AuthenticationRequired
    );

    let mut headers = HeaderMap::new();
    headers.insert(AUTHORIZATION, HeaderValue::from_static("Basic abc"));
    assert_eq!(
        bearer_token(&headers).unwrap_err().code,
        ErrorCode::AuthenticationFailed
    );
    headers.insert(AUTHORIZATION, HeaderValue::from_bytes(&[0x80]).unwrap());
    assert_eq!(
        bearer_token(&headers).unwrap_err().message,
        "authorization header is malformed"
    );
    headers.insert(AUTHORIZATION, HeaderValue::from_static("Bearer "));
    assert_eq!(bearer_token(&headers).unwrap(), "");

    let mut headers = HeaderMap::new();
    headers.insert(
        SEC_WEBSOCKET_PROTOCOL,
        HeaderValue::from_static("loom.bearer."),
    );
    assert_eq!(
        request_auth(&headers).unwrap_err().code,
        ErrorCode::AuthenticationRequired
    );
}

#[test]
fn authentication_response_and_socket_errors_map_to_stable_codes() {
    let unauthorized = auth_error_response(loom_core::LoomError::new(
        ErrorCode::AuthenticationRequired,
        "missing",
        false,
    ))
    .into_response();
    assert_eq!(unauthorized.status(), axum::http::StatusCode::UNAUTHORIZED);
    let forbidden = auth_error_response(loom_core::LoomError::new(
        ErrorCode::AuthorizationDenied,
        "denied",
        false,
    ))
    .into_response();
    assert_eq!(forbidden.status(), axum::http::StatusCode::FORBIDDEN);

    for (message, code) in [
        ("timed out", ErrorCode::DeadlineExceeded),
        ("connection refused", ErrorCode::ProviderUnavailable),
        ("unexpected network failure", ErrorCode::Internal),
    ] {
        assert_eq!(
            websocket_connect_error(TungsteniteError::Io(io::Error::other(message))).code,
            code
        );
    }

    let feature_disabled =
        websocket_connect_error(TungsteniteError::Url(UrlError::TlsFeatureNotEnabled));
    assert!(
        feature_disabled.message.contains("without TLS support"),
        "{feature_disabled}"
    );
    let unknown_issuer = websocket_connect_error(TungsteniteError::Io(io::Error::other(
        "invalid peer certificate: UnknownIssuer",
    )));
    assert!(
        unknown_issuer.message.contains("TLS handshake"),
        "{unknown_issuer}"
    );
    assert!(
        unknown_issuer.message.contains("UnknownIssuer"),
        "{unknown_issuer}"
    );
    assert!(!unknown_issuer.retryable);
}

#[tokio::test]
async fn remote_server_rejects_invalid_paths_and_zero_limits() {
    let backend = InProcessBackend::new();
    let auth = std::sync::Arc::new(AuthTokenStore::new());
    let mut config = RemoteServerConfig::local_ephemeral();
    config.path.clear();
    let error = RemoteServer::new(backend.clone(), auth.clone(), config)
        .bind()
        .await
        .err()
        .unwrap();
    assert_eq!(error.code, ErrorCode::InvalidRequest);

    for field in 0..6 {
        let mut config = RemoteServerConfig::local_ephemeral();
        match field {
            0 => config.heartbeat_interval = Duration::ZERO,
            1 => config.request_timeout = Duration::ZERO,
            2 => config.max_frame_bytes = 0,
            3 => config.max_in_flight_requests = 0,
            4 => config.max_in_flight_control_requests = 0,
            _ => config.outgoing_capacity = 0,
        }
        let error = RemoteServer::new(backend.clone(), auth.clone(), config)
            .bind()
            .await
            .err()
            .unwrap();
        assert_eq!(error.code, ErrorCode::InvalidRequest);
    }
    let mut config = RemoteServerConfig::local_ephemeral();
    config.path = "ws".to_owned();
    assert_eq!(
        RemoteServer::new(backend, auth, config)
            .bind()
            .await
            .err()
            .unwrap()
            .code,
        ErrorCode::InvalidRequest
    );
}

#[tokio::test]
async fn remote_frame_processing_rejects_large_malformed_and_cancel_frames() {
    let backend = InProcessBackend::new();
    let connection = backend.connect();
    let (outgoing, mut received) = mpsc::channel(4);
    let (control, _) = mpsc::channel(1);
    let mut pending = HashMap::new();

    let close = process_text(
        b"too large",
        &connection,
        &outgoing,
        &control,
        &mut pending,
        &test_limits(1),
    )
    .await
    .expect("a frame that names no request must close the connection");
    assert_eq!(close.code, CLOSE_MESSAGE_TOO_BIG);
    assert_eq!(
        close.reason,
        frame_too_large_close_reason(b"too large".len(), 1)
    );
    assert!(received.try_recv().is_err());

    // A frame that still decodes is answered under the request id it names,
    // so the client sees the failure on the request it sent.
    let request =
        RequestEnvelope::new(ClientRequest::Control(ControlRequest::DiscoverCapabilities));
    let encoded = encode_request(&request).unwrap();
    let limit = encoded.len() - 1;
    let close = process_text(
        &encoded,
        &connection,
        &outgoing,
        &control,
        &mut pending,
        &test_limits(limit),
    )
    .await;
    assert!(close.is_none());
    let Some(axum::extract::ws::Message::Text(text)) = received.recv().await else {
        panic!("expected an oversized-request response")
    };
    let response = decode_response(text.as_bytes()).unwrap();
    assert_eq!(response.request_id, request.request_id);
    assert_eq!(
        response.result.unwrap_err(),
        frame_too_large_error("WebSocket frame", request.request_id, encoded.len(), limit)
    );

    process_text(
        b"invalid",
        &connection,
        &outgoing,
        &control,
        &mut pending,
        &test_limits(100),
    )
    .await;
    let Some(axum::extract::ws::Message::Text(text)) = received.recv().await else {
        panic!("expected malformed-frame response")
    };
    assert_eq!(
        decode_response(text.as_bytes())
            .unwrap()
            .result
            .unwrap_err()
            .code,
        ErrorCode::MalformedPayload
    );

    let request_id = RequestId::new();
    let cancel = encode_client_frame(&ClientFrame::Cancel { request_id }).unwrap();
    process_text(
        &cancel,
        &connection,
        &outgoing,
        &control,
        &mut pending,
        &test_limits(100),
    )
    .await;
    let Some(axum::extract::ws::Message::Text(text)) = received.recv().await else {
        panic!("expected cancelled-request response")
    };
    let response = decode_response(text.as_bytes()).unwrap();
    assert_eq!(response.request_id, request_id);
    assert_eq!(
        response.result.unwrap_err().code,
        ErrorCode::RequestCancelled
    );

    let request =
        RequestEnvelope::new(ClientRequest::Control(ControlRequest::DiscoverCapabilities));
    let wrapped = encode_client_frame(&ClientFrame::Request(Box::new(request))).unwrap();
    process_text(
        &wrapped,
        &connection,
        &outgoing,
        &control,
        &mut pending,
        &test_limits(1024),
    )
    .await;
    let Some(axum::extract::ws::Message::Text(text)) = received.recv().await else {
        panic!("expected wrapped request response")
    };
    assert!(decode_response(text.as_bytes()).is_ok());
}

#[test]
fn frame_size_diagnostics_name_the_request_size_and_limit() {
    let request_id = RequestId::new();
    let error = frame_too_large_error("remote response", request_id, 5120, 4096);
    assert_eq!(error.code, ErrorCode::PayloadTooLarge);
    assert_eq!(
        error.message,
        format!(
            "remote response of 5120 bytes for request {request_id} is above the configured \
                 frame size limit of 4096 bytes"
        )
    );
    assert!(!error.retryable);
    assert_eq!(
        frame_too_large_close_reason(5120, 4096),
        "frame of 5120 bytes over the configured 4096-byte limit"
    );
    // A close reason has to fit the 123-byte WebSocket close payload.
    assert!(frame_too_large_close_reason(usize::MAX, 1).len() <= 123);
}

#[test]
fn the_frame_limit_resolves_from_a_flag_then_the_environment() {
    assert_eq!(DEFAULT_MAX_FRAME_BYTES, 4 * 1024 * 1024);
    assert_eq!(
        resolve_max_frame_bytes(None, None).unwrap(),
        DEFAULT_MAX_FRAME_BYTES
    );
    assert_eq!(resolve_max_frame_bytes(Some(" 8192 "), None).unwrap(), 8192);
    assert_eq!(resolve_max_frame_bytes(None, Some("4096")).unwrap(), 4096);
    // An explicit flag wins over the environment it falls back to.
    assert_eq!(
        resolve_max_frame_bytes(Some("8192"), Some("4096")).unwrap(),
        8192
    );

    let error = resolve_max_frame_bytes(None, Some("many")).unwrap_err();
    assert_eq!(error.code, ErrorCode::InvalidRequest);
    assert!(error.message.contains(MAX_FRAME_BYTES_ENV), "{error}");
    let error = resolve_max_frame_bytes(Some("0"), None).unwrap_err();
    assert_eq!(error.code, ErrorCode::InvalidRequest);
    assert!(error.message.contains(MAX_FRAME_BYTES_FLAG), "{error}");
    assert!(error.message.contains("greater than zero"), "{error}");
}

#[test]
fn oversized_inbound_frames_fail_only_the_request_they_answer() {
    let awaited = RequestId::new();
    let awaited_response = encode_response(&ResponseEnvelope::failure(
        awaited,
        loom_core::LoomError::invalid_request("await"),
    ))
    .unwrap();
    assert_eq!(
        oversized_inbound_frame_action(&awaited_response, awaited),
        OversizedFrameAction::FailAwaitedRequest
    );

    // Another request's response, a payload that is not a response at all,
    // and bytes that decode as nothing are drained instead.
    let other_response = encode_response(&ResponseEnvelope::failure(
        RequestId::new(),
        loom_core::LoomError::invalid_request("other"),
    ))
    .unwrap();
    assert_eq!(
        oversized_inbound_frame_action(&other_response, awaited),
        OversizedFrameAction::Drain
    );
    for frame in [b"{not-json".as_slice(), b"too large".as_slice()] {
        assert_eq!(
            oversized_inbound_frame_action(frame, awaited),
            OversizedFrameAction::Drain
        );
    }
}

#[test]
fn oversized_frames_are_correlated_when_they_name_a_request() {
    let request =
        RequestEnvelope::new(ClientRequest::Control(ControlRequest::DiscoverCapabilities));
    assert_eq!(
        frame_request_id(&encode_request(&request).unwrap()),
        Some(request.request_id)
    );
    let wrapped = encode_client_frame(&ClientFrame::Request(Box::new(request.clone()))).unwrap();
    assert_eq!(frame_request_id(&wrapped), Some(request.request_id));

    let request_id = RequestId::new();
    let cancel = encode_client_frame(&ClientFrame::Cancel { request_id }).unwrap();
    assert_eq!(frame_request_id(&cancel), Some(request_id));
    assert_eq!(frame_request_id(b"too large"), None);
}

#[tokio::test]
async fn remote_frame_processing_tracks_plain_requests_and_cancels_pending_work() {
    let backend = InProcessBackend::new();
    let connection = backend.connect();
    let (outgoing, mut received) = mpsc::channel(4);
    let (control, _) = mpsc::channel(1);
    let mut pending = HashMap::new();

    let request =
        RequestEnvelope::new(ClientRequest::Control(ControlRequest::DiscoverCapabilities));
    let request_id = request.request_id;
    process_text(
        &encode_request(&request).unwrap(),
        &connection,
        &outgoing,
        &control,
        &mut pending,
        &test_limits(1024),
    )
    .await;
    assert_eq!(pending.len(), 1);

    let cancel = encode_client_frame(&ClientFrame::Cancel { request_id }).unwrap();
    process_text(
        &cancel,
        &connection,
        &outgoing,
        &control,
        &mut pending,
        &test_limits(1024),
    )
    .await;
    let Some(Message::Text(text)) = received.recv().await else {
        panic!("expected cancellation response")
    };
    let response = decode_response(text.as_bytes()).unwrap();
    assert_eq!(response.request_id, request_id);
    assert_eq!(
        response.result.unwrap_err().code,
        ErrorCode::RequestCancelled
    );
    assert!(pending.is_empty());

    let timed_out = execute_request_with(RequestId::new(), Duration::ZERO, || {
        std::thread::sleep(Duration::from_millis(10));
        ResponseEnvelope::failure(
            RequestId::new(),
            loom_core::LoomError::invalid_request("late response"),
        )
    })
    .await;
    assert_eq!(
        timed_out.result.unwrap_err().code,
        ErrorCode::DeadlineExceeded
    );
}

#[test]
fn the_in_flight_limits_start_from_the_defaults() {
    assert_eq!(DEFAULT_MAX_IN_FLIGHT_REQUESTS, 8);
    assert_eq!(DEFAULT_MAX_IN_FLIGHT_CONTROL_REQUESTS, 4);
    let config = RemoteServerConfig::default();
    assert_eq!(config.max_in_flight_requests, 8);
    assert_eq!(config.max_in_flight_control_requests, 4);
    let limits = ConnectionLimits::new(&config);
    assert_eq!(limits.max_frame_bytes, config.max_frame_bytes);
    assert_eq!(limits.request_timeout, config.request_timeout);
    assert_eq!(limits.in_flight.max_ordinary, 8);
    assert_eq!(limits.in_flight.max_control, 4);
}

#[test]
fn control_and_interrupt_requests_are_classified_apart_from_ordinary_ones() {
    for request in [
        ClientRequest::Control(ControlRequest::DiscoverCapabilities),
        ClientRequest::Run(RunRequest::InterruptAgentRun {
            run_id: loom_core::RunId::new(),
        }),
    ] {
        assert!(is_control_or_interrupt_request(&request), "{request:?}");
        assert_eq!(RequestClass::of(&request), RequestClass::Control);
    }
    for request in [
        ClientRequest::Workspace(WorkspaceRequest::CreateWorkspace {
            name: "workspace".to_owned(),
        }),
        ClientRequest::Provider(loom_protocol::ProviderRequest::ListModels),
    ] {
        assert!(!is_control_or_interrupt_request(&request), "{request:?}");
        assert_eq!(RequestClass::of(&request), RequestClass::Ordinary);
    }
}

#[tokio::test]
async fn requests_past_the_in_flight_limit_are_refused_and_never_silently_dropped() {
    let backend = InProcessBackend::new();
    let connection = backend.connect();
    let (outgoing, mut received) = mpsc::channel(8);
    let (control, _) = mpsc::channel(1);
    let mut pending = HashMap::new();
    let limits = ConnectionLimits {
        max_frame_bytes: 4096,
        request_timeout: Duration::from_secs(1),
        in_flight: InFlightLimits::new(1, 1),
    };
    // An in-flight ordinary request holds the connection's only ordinary
    // permit for as long as its blocking operation runs.
    let held = limits
        .in_flight
        .admit(RequestId::new(), RequestClass::Ordinary)
        .unwrap();

    let request = RequestEnvelope::new(ClientRequest::Workspace(
        WorkspaceRequest::CreateWorkspace {
            name: "pipelined".to_owned(),
        },
    ));
    let request_id = request.request_id;
    process_text(
        &encode_request(&request).unwrap(),
        &connection,
        &outgoing,
        &control,
        &mut pending,
        &limits,
    )
    .await;
    let Some(Message::Text(text)) = received.recv().await else {
        panic!("a refused request must be answered, not dropped")
    };
    let response = decode_response(text.as_bytes()).unwrap();
    assert_eq!(response.request_id, request_id);
    let error = response.result.unwrap_err();
    assert_eq!(error.code, ErrorCode::Backpressure);
    assert!(error.retryable);
    assert_eq!(
        error.message,
        format!(
            "request {request_id} was not started: this connection is at its configured limit \
                 of 1 in-flight requests (max_in_flight_requests); wait for an outstanding \
                 response before sending another"
        )
    );
    // A refused request never starts, so it holds no slot and no task.
    assert!(pending.is_empty());

    // The pools are separate, so a control request is admitted and answered
    // while the ordinary pool is exhausted.
    let control_request =
        RequestEnvelope::new(ClientRequest::Control(ControlRequest::DiscoverCapabilities));
    process_text(
        &encode_request(&control_request).unwrap(),
        &connection,
        &outgoing,
        &control,
        &mut pending,
        &limits,
    )
    .await;
    assert_eq!(pending.len(), 1);
    let Some(Message::Text(text)) = received.recv().await else {
        panic!("a control request must be admitted while ordinary requests are at their limit")
    };
    let response = decode_response(text.as_bytes()).unwrap();
    assert_eq!(response.request_id, control_request.request_id);
    assert!(response.result.is_ok());
    drop(held);

    // The control pool is bounded too, so no request class can queue
    // without limit.
    let control_held = limits
        .in_flight
        .admit(RequestId::new(), RequestClass::Control)
        .unwrap();
    let second = RequestEnvelope::new(ClientRequest::Control(ControlRequest::DiscoverCapabilities));
    let second_id = second.request_id;
    process_text(
        &encode_request(&second).unwrap(),
        &connection,
        &outgoing,
        &control,
        &mut pending,
        &limits,
    )
    .await;
    let Some(Message::Text(text)) = received.recv().await else {
        panic!("a control request past its limit must be answered")
    };
    let error = decode_response(text.as_bytes())
        .unwrap()
        .result
        .unwrap_err();
    assert_eq!(error.code, ErrorCode::Backpressure);
    assert_eq!(
        error.message,
        format!(
            "request {second_id} was not started: this connection is at its configured limit \
                 of 1 in-flight control requests (max_in_flight_control_requests); wait for an \
                 outstanding response before sending another"
        )
    );
    drop(control_held);
}

#[tokio::test]
async fn cancelling_a_request_reports_that_its_operation_may_still_complete() {
    let backend = InProcessBackend::new();
    let connection = backend.connect();
    let (outgoing, mut received) = mpsc::channel(4);
    let (control, _) = mpsc::channel(1);
    let mut pending = HashMap::new();

    let request =
        RequestEnvelope::new(ClientRequest::Control(ControlRequest::DiscoverCapabilities));
    let request_id = request.request_id;
    process_text(
        &encode_request(&request).unwrap(),
        &connection,
        &outgoing,
        &control,
        &mut pending,
        &test_limits(1024),
    )
    .await;

    let cancel = encode_client_frame(&ClientFrame::Cancel { request_id }).unwrap();
    process_text(
        &cancel,
        &connection,
        &outgoing,
        &control,
        &mut pending,
        &test_limits(1024),
    )
    .await;
    let Some(Message::Text(text)) = received.recv().await else {
        panic!("expected a response to the cancel frame")
    };
    let response = decode_response(text.as_bytes()).unwrap();
    assert_eq!(response.request_id, request_id);
    let error = response.result.unwrap_err();
    // The client is told the request was abandoned rather than undone,
    // because the blocking operation is not stopped by cancelling the wait.
    assert_eq!(error.code, ErrorCode::RequestCancelled);
    assert_eq!(
        error,
        abandoned_request_error(
            ErrorCode::RequestCancelled,
            request_id,
            "by the client",
            false
        )
    );
    assert!(error.message.contains("may still complete"), "{error}");
    assert!(error.message.contains("abandoned by the client"), "{error}");
    assert!(pending.is_empty());
}

#[tokio::test]
async fn aborting_a_request_does_not_stop_its_blocking_operation() {
    // A cancel frame answers by aborting the task that waits for the
    // operation. This pins what that abort does and does not stop: the
    // response above says the effect may still land, and here it does.
    let request_id = RequestId::new();
    let started = Arc::new(AtomicBool::new(false));
    let landed = Arc::new(AtomicBool::new(false));
    let operation = {
        let started = Arc::clone(&started);
        let landed = Arc::clone(&landed);
        move || {
            started.store(true, Ordering::SeqCst);
            std::thread::sleep(Duration::from_millis(50));
            landed.store(true, Ordering::SeqCst);
            ResponseEnvelope::failure(
                request_id,
                loom_core::LoomError::invalid_request("late response"),
            )
        }
    };
    let task = tokio::spawn(execute_request_with(
        request_id,
        Duration::from_secs(30),
        operation,
    ));
    while !started.load(Ordering::SeqCst) {
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    task.abort();

    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while !landed.load(Ordering::SeqCst) && std::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert!(
        landed.load(Ordering::SeqCst),
        "aborting the waiting task must not stop the blocking operation"
    );
}

#[tokio::test]
async fn a_timed_out_request_reports_that_its_operation_may_still_complete() {
    let request_id = RequestId::new();
    let landed = Arc::new(AtomicBool::new(false));
    let operation = {
        let landed = Arc::clone(&landed);
        move || {
            std::thread::sleep(Duration::from_millis(50));
            landed.store(true, Ordering::SeqCst);
            ResponseEnvelope::failure(
                request_id,
                loom_core::LoomError::invalid_request("late response"),
            )
        }
    };
    let timed_out = execute_request_with(request_id, Duration::from_millis(5), operation).await;
    let error = timed_out.result.unwrap_err();
    assert_eq!(error.code, ErrorCode::DeadlineExceeded);
    assert!(error.retryable);
    assert_eq!(
        error.message,
        format!(
            "request {request_id} was abandoned when its 5ms request deadline passed; the \
                 operation runs on a blocking thread that abandoning the wait does not stop, so \
                 it may still complete and its effect may still be applied; read the state again \
                 before assuming it did not happen"
        )
    );
    assert!(error.message.contains("may still complete"), "{error}");

    // The rule is honest: the abandoned operation really does keep running
    // and apply its effect after the caller was answered.
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while !landed.load(Ordering::SeqCst) && std::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert!(
        landed.load(Ordering::SeqCst),
        "the deadline must not stop the blocking operation"
    );
}

#[tokio::test]
async fn outgoing_backpressure_and_disconnect_are_reported() {
    let response = ResponseEnvelope::failure(
        RequestId::new(),
        loom_core::LoomError::invalid_request("test"),
    );
    let (outgoing, received) = mpsc::channel(1);
    let (control, mut controls) = mpsc::channel(1);
    outgoing
        .send(axum::extract::ws::Message::Ping(Vec::new().into()))
        .await
        .unwrap();
    assert_eq!(
        try_send_response(&outgoing, response.clone(), &control)
            .unwrap_err()
            .code,
        ErrorCode::Backpressure
    );
    assert!(matches!(
        controls.try_recv(),
        Ok(ConnectionControl::Backpressure)
    ));
    drop(received);
    let (closed_outgoing, closed_receiver) = mpsc::channel(1);
    drop(closed_receiver);
    assert_eq!(
        try_send_response(&closed_outgoing, response, &control)
            .unwrap_err()
            .code,
        ErrorCode::RequestCancelled
    );
}

#[test]
fn remote_server_accessors_and_transport_configuration_are_available() {
    let backend = InProcessBackend::new();
    let auth = std::sync::Arc::new(AuthTokenStore::new());
    let config = RemoteServerConfig::local_ephemeral();
    let server = RemoteServer::new(backend.clone(), auth.clone(), config.clone());
    assert!(std::sync::Arc::ptr_eq(server.backend(), &backend));
    assert!(std::sync::Arc::ptr_eq(server.auth(), &auth));
    assert_eq!(server.config().path, config.path);

    let transport = WebSocketTransport::new("ws://worker.example/ws", "private-token");
    assert_eq!(transport.url(), "ws://worker.example/ws");
    assert!(!format!("{transport:?}").contains("private-token"));
    assert_eq!(
        transport
            .clone()
            .with_max_frame_bytes(0)
            .err()
            .unwrap()
            .code,
        ErrorCode::InvalidRequest
    );
    assert_eq!(
        transport.with_max_frame_bytes(16).unwrap().max_frame_bytes,
        16
    );
}

#[tokio::test]
async fn transport_rejects_invalid_urls_and_bearer_headers() {
    let invalid_url = WebSocketTransport::new("not a websocket URL", "token");
    assert_eq!(
        invalid_url.connect().await.err().unwrap().code,
        ErrorCode::InvalidRequest
    );
    let invalid_token = WebSocketTransport::new("ws://localhost:1/ws", "bad\ntoken");
    assert_eq!(
        invalid_token.connect().await.err().unwrap().code,
        ErrorCode::InvalidRequest
    );
}

#[test]
fn websocket_urls_format_ipv6_addresses() {
    let mut config = RemoteServerConfig::local_ephemeral();
    config.path = "/custom".to_owned();
    assert_eq!(
        config.websocket_url(SocketAddr::new(IpAddr::V6(Ipv6Addr::LOCALHOST), 1234)),
        "ws://[::1]:1234/custom"
    );
    assert!(config.is_local_only());
    config.bind_addr = SocketAddr::new(IpAddr::V6("2001:db8::1".parse().unwrap()), 1234);
    assert!(!config.is_local_only());
}

#[test]
fn configured_tls_is_advertised_as_wss() {
    let mut config = RemoteServerConfig::local_ephemeral();
    assert!(!config.tls_enabled());
    config.tls = Some(test_tls());
    assert!(config.tls_enabled());
    assert_eq!(
        config.websocket_url(SocketAddr::new(
            IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
            8765
        )),
        "wss://127.0.0.1:8765/ws"
    );
    const { assert!(WEBSOCKET_TLS_SUPPORTED) };
}

#[test]
fn plaintext_remote_hosts_need_the_insecure_opt_in() {
    assert_eq!(
        insecure_remote_host("ws://worker.example:8765/ws").as_deref(),
        Some("worker.example")
    );
    assert_eq!(
        insecure_remote_host("ws://192.0.2.10:8765/ws").as_deref(),
        Some("192.0.2.10")
    );
    assert_eq!(insecure_remote_host("wss://worker.example/ws"), None);
    assert_eq!(insecure_remote_host("ws://localhost:8765/ws"), None);
    assert_eq!(insecure_remote_host("ws://127.0.0.1:8765/ws"), None);
    assert_eq!(insecure_remote_host("ws://[::1]:8765/ws"), None);
    assert_eq!(insecure_remote_host("not a websocket URL"), None);
}

#[tokio::test]
async fn transport_refuses_plaintext_remote_endpoints_without_the_opt_in() {
    let error = WebSocketTransport::new("ws://worker.example:8765/ws", "private-token")
        .connect()
        .await
        .err()
        .expect("plaintext remote connections must be refused");
    assert_eq!(error.code, ErrorCode::InvalidRequest);
    assert!(error.message.contains("--allow-insecure-remote"), "{error}");
    assert!(!error.message.contains("private-token"), "{error}");
}

#[tokio::test]
async fn loopback_binds_serve_plaintext_without_tls_or_an_opt_in() {
    let backend = InProcessBackend::new();
    let auth = std::sync::Arc::new(AuthTokenStore::new());
    let server = RemoteServer::new(backend, auth, RemoteServerConfig::local_ephemeral())
        .bind()
        .await
        .unwrap();
    assert!(!server.tls_enabled());
    assert!(server.websocket_url().starts_with("ws://127.0.0.1:"));
    assert_eq!(
        server.health_url(),
        format!("http://{}/health", server.local_addr())
    );
    server.stop().await.unwrap();
}

#[tokio::test]
async fn non_loopback_plaintext_binds_need_tls_or_the_opt_in() {
    let backend = InProcessBackend::new();
    let auth = std::sync::Arc::new(AuthTokenStore::new());
    let config = |allow_insecure_remote, tls| RemoteServerConfig {
        bind_addr: "0.0.0.0:0".parse().unwrap(),
        allow_insecure_remote,
        tls,
        ..RemoteServerConfig::default()
    };

    let error = RemoteServer::new(backend.clone(), auth.clone(), config(false, None))
        .bind()
        .await
        .err()
        .expect("plaintext non-loopback binds must be refused");
    assert_eq!(error.code, ErrorCode::InvalidRequest);
    assert!(error.message.contains("TLS"), "{error}");

    let server = RemoteServer::new(
        backend.clone(),
        auth.clone(),
        config(false, Some(test_tls())),
    )
    .bind()
    .await
    .unwrap();
    assert!(server.tls_enabled());
    assert!(server.websocket_url().starts_with("wss://0.0.0.0:"));
    assert!(server.health_url().starts_with("https://"));
    server.stop().await.unwrap();

    let server = RemoteServer::new(backend, auth, config(true, None))
        .bind()
        .await
        .unwrap();
    assert!(!server.tls_enabled());
    server.stop().await.unwrap();
}

#[tokio::test]
async fn tls_listener_serves_health_and_the_websocket_protocol() {
    let backend = InProcessBackend::new();
    let auth = std::sync::Arc::new(AuthTokenStore::new());
    let token = auth.issue(AuthorizationScope::all()).unwrap();
    let mut config = RemoteServerConfig::local_ephemeral();
    config.tls = Some(test_tls());
    let server = RemoteServer::new(backend, auth, config)
        .bind()
        .await
        .unwrap();
    assert!(server.tls_enabled());
    assert!(server.websocket_url().starts_with("wss://127.0.0.1:"));

    let health = tls_request(server.local_addr(), "/health").await;
    assert!(health.starts_with("HTTP/1.1 200 OK"), "{health}");
    assert!(health.ends_with("ok"), "{health}");

    let mut connection = WebSocketTransport::new(server.websocket_url(), token.token)
        .with_ca_certificate_pem(TEST_CA_PEM)
        .unwrap()
        .connect()
        .await
        .unwrap();
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

    drop(connection);
    server.stop().await.unwrap();
}

#[tokio::test]
async fn health_reports_a_persistence_fail_stop() {
    let backend = InProcessBackend::new();
    let state = RemoteState {
        backend: Arc::clone(&backend),
        auth: Arc::new(AuthTokenStore::new()),
        config: RemoteServerConfig::local_ephemeral(),
        anonymous: Arc::new(Semaphore::new(DEFAULT_MAX_ANONYMOUS_CONNECTIONS)),
    };
    let healthy = health_handler(State(state.clone())).await.into_response();
    assert_eq!(healthy.status(), StatusCode::OK);

    // A backend that latched its persistence fail-stop refuses durable
    // writes, so the listener must stop reporting health and let an
    // orchestrator restart it.
    backend.persistence_failed.store(true, Ordering::SeqCst);
    let stopped = health_handler(State(state)).await.into_response();
    assert_eq!(stopped.status(), StatusCode::SERVICE_UNAVAILABLE);
}

#[test]
fn server_tls_material_is_validated() {
    assert!(
        ServerTlsConfig::from_pem(TEST_CERT_PEM, TEST_KEY_PEM)
            .server_config()
            .is_ok()
    );
    let no_certificates = ServerTlsConfig::from_pem(Vec::new(), TEST_KEY_PEM)
        .server_config()
        .unwrap_err();
    assert!(
        no_certificates.message.contains("no certificates"),
        "{no_certificates}"
    );
    let no_private_key = ServerTlsConfig::from_pem(TEST_CERT_PEM, Vec::new())
        .server_config()
        .unwrap_err();
    assert!(
        no_private_key.message.contains("no private key"),
        "{no_private_key}"
    );
    assert_eq!(
        ServerTlsConfig::from_pem(b"junk".to_vec(), b"junk".to_vec())
            .server_config()
            .unwrap_err()
            .code,
        ErrorCode::InvalidRequest
    );
}

#[test]
fn server_tls_material_loads_from_files_and_reports_missing_ones() {
    let tls =
        ServerTlsConfig::from_pem_files(fixture("server-cert.pem"), fixture("server-key.pem"))
            .unwrap();
    assert!(tls.server_config().is_ok());
    assert!(!format!("{tls:?}").contains("PRIVATE KEY"));

    let error = ServerTlsConfig::from_pem_files(fixture("missing.pem"), fixture("server-key.pem"))
        .unwrap_err();
    assert_eq!(error.code, ErrorCode::InvalidRequest);
    assert!(
        error
            .message
            .contains("could not read the TLS certificate file"),
        "{error}"
    );
    let error = ServerTlsConfig::from_pem_files(fixture("server-cert.pem"), fixture("missing.pem"))
        .unwrap_err();
    assert!(
        error
            .message
            .contains("could not read the TLS private key file"),
        "{error}"
    );
}

#[test]
fn transport_rejects_unusable_ca_material() {
    for pem in [Vec::new(), b"junk".to_vec()] {
        let error = WebSocketTransport::new("wss://worker.example/ws", "token")
            .with_ca_certificate_pem(pem)
            .expect_err("unusable CA material must be rejected");
        assert_eq!(error.code, ErrorCode::InvalidRequest);
    }
    let error = WebSocketTransport::new("wss://worker.example/ws", "token")
        .with_ca_certificate_file(fixture("missing-ca.pem"))
        .expect_err("a missing CA file must be rejected");
    assert!(
        error.message.contains("could not read the TLS CA file"),
        "{error}"
    );
    assert!(
        WebSocketTransport::new("wss://worker.example/ws", "token")
            .with_ca_certificate_pem(TEST_CA_PEM)
            .is_ok()
    );
}

#[tokio::test]
async fn insecure_remote_opt_in_permits_a_plaintext_connection() {
    let backend = InProcessBackend::new();
    let auth = std::sync::Arc::new(AuthTokenStore::new());
    let token = auth.issue(AuthorizationScope::all()).unwrap();
    let server = RemoteServer::new(
        backend,
        auth,
        RemoteServerConfig {
            // `0.0.0.0` is not loopback, so both sides have to opt in; on
            // Linux a connection to `0.0.0.0` reaches the local listener,
            // which keeps the test offline and deterministic.
            bind_addr: "0.0.0.0:0".parse().unwrap(),
            allow_insecure_remote: true,
            ..RemoteServerConfig::default()
        },
    )
    .bind()
    .await
    .unwrap();
    let url = server.websocket_url().to_owned();

    let refused = WebSocketTransport::new(url.clone(), token.token.clone())
        .connect()
        .await
        .err()
        .expect("a plaintext non-loopback endpoint must be refused without the opt-in");
    assert!(
        refused.message.contains("--allow-insecure-remote"),
        "{refused}"
    );

    let mut connection = WebSocketTransport::new(url.clone(), token.token)
        .with_insecure_remote_allowed(true)
        .connect()
        .await
        .unwrap_or_else(|error| {
            panic!("the insecure-remote opt-in should permit '{url}': {error}")
        });
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

    drop(connection);
    server.stop().await.unwrap();
}

/// Issues one HTTP request over TLS, trusting the test CA.
async fn tls_request(address: SocketAddr, path: &str) -> String {
    let connector = tokio_rustls::TlsConnector::from(client_tls_config(TEST_CA_PEM).unwrap());
    let server_name = rustls::pki_types::ServerName::try_from("127.0.0.1")
        .unwrap()
        .to_owned();
    let stream = TcpStream::connect(address).await.unwrap();
    let mut stream = connector.connect(server_name, stream).await.unwrap();
    let request = format!("GET {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n");
    stream.write_all(request.as_bytes()).await.unwrap();
    let mut response = Vec::new();
    stream.read_to_end(&mut response).await.unwrap();
    String::from_utf8_lossy(&response).into_owned()
}
