use std::{
    collections::HashMap,
    fmt,
    net::{IpAddr, SocketAddr},
    sync::Arc,
    time::Duration,
};

use axum::{
    Router,
    extract::{
        State,
        ws::{Message, WebSocket, WebSocketUpgrade},
    },
    http::{
        HeaderMap, StatusCode,
        header::{AUTHORIZATION, SEC_WEBSOCKET_PROTOCOL},
    },
    response::{IntoResponse, Response},
    routing::get,
};
use futures_util::{SinkExt, StreamExt};
use loom_core::{ErrorCode, LoomError, RequestId, Result};
use loom_protocol::{
    ClientFrame, RequestEnvelope, ResponseEnvelope, ServerEventEnvelope, decode_client_frame,
    decode_event, decode_request, encode_client_frame, encode_response,
};
use tokio::{
    net::TcpListener,
    sync::{mpsc, oneshot},
    task::{JoinHandle, spawn_blocking},
    time::{Instant, interval, timeout},
};
use tokio_tungstenite::{
    MaybeTlsStream, connect_async,
    tungstenite::{Error as TungsteniteError, Message as ClientMessage, client::IntoClientRequest},
};

use super::{AuthSession, AuthTokenStore, InProcessBackend, InProcessConnection};

const DEFAULT_WS_PATH: &str = "/ws";
const DEFAULT_HEARTBEAT: Duration = Duration::from_secs(15);
const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const DEFAULT_MAX_FRAME_BYTES: usize = 4 * 1024 * 1024;
const DEFAULT_OUTGOING_CAPACITY: usize = 128;

#[derive(Clone, Debug)]
pub struct RemoteServerConfig {
    pub bind_addr: SocketAddr,
    pub path: String,
    pub heartbeat_interval: Duration,
    pub request_timeout: Duration,
    pub max_frame_bytes: usize,
    pub outgoing_capacity: usize,
}

impl Default for RemoteServerConfig {
    fn default() -> Self {
        Self {
            bind_addr: SocketAddr::new(IpAddr::V4(std::net::Ipv4Addr::LOCALHOST), 8765),
            path: DEFAULT_WS_PATH.to_owned(),
            heartbeat_interval: DEFAULT_HEARTBEAT,
            request_timeout: DEFAULT_REQUEST_TIMEOUT,
            max_frame_bytes: DEFAULT_MAX_FRAME_BYTES,
            outgoing_capacity: DEFAULT_OUTGOING_CAPACITY,
        }
    }
}

impl RemoteServerConfig {
    pub fn local_ephemeral() -> Self {
        Self {
            bind_addr: SocketAddr::new(IpAddr::V4(std::net::Ipv4Addr::LOCALHOST), 0),
            ..Self::default()
        }
    }

    pub fn is_local_only(&self) -> bool {
        self.bind_addr.ip().is_loopback()
    }

    pub fn websocket_url(&self, address: SocketAddr) -> String {
        let host = match address.ip() {
            IpAddr::V4(address) => address.to_string(),
            IpAddr::V6(address) => format!("[{address}]"),
        };
        format!("ws://{host}:{}{}", address.port(), self.path)
    }
}

#[derive(Clone)]
pub struct RemoteServer {
    backend: Arc<InProcessBackend>,
    auth: Arc<AuthTokenStore>,
    config: RemoteServerConfig,
}

impl RemoteServer {
    pub fn new(
        backend: Arc<InProcessBackend>,
        auth: Arc<AuthTokenStore>,
        config: RemoteServerConfig,
    ) -> Self {
        Self {
            backend,
            auth,
            config,
        }
    }

    pub fn backend(&self) -> &Arc<InProcessBackend> {
        &self.backend
    }

    pub fn auth(&self) -> &Arc<AuthTokenStore> {
        &self.auth
    }

    pub fn config(&self) -> &RemoteServerConfig {
        &self.config
    }

    pub async fn bind(self) -> Result<RunningRemoteServer> {
        if self.config.path.is_empty() || !self.config.path.starts_with('/') {
            return Err(LoomError::invalid_request(
                "WebSocket path must be non-empty and start with '/'",
            ));
        }
        if self.config.heartbeat_interval.is_zero()
            || self.config.request_timeout.is_zero()
            || self.config.max_frame_bytes == 0
            || self.config.outgoing_capacity == 0
        {
            return Err(LoomError::invalid_request(
                "remote server limits must be greater than zero",
            ));
        }
        let listener = TcpListener::bind(self.config.bind_addr)
            .await
            .map_err(|error| {
                LoomError::new(
                    ErrorCode::Internal,
                    format!("could not bind Loom WebSocket server: {error}"),
                    true,
                )
            })?;
        let local_addr = listener.local_addr().map_err(|error| {
            LoomError::new(
                ErrorCode::Internal,
                format!("could not inspect Loom WebSocket listener: {error}"),
                true,
            )
        })?;
        let state = RemoteState {
            backend: self.backend,
            auth: self.auth,
            config: self.config.clone(),
        };
        let app = Router::new()
            .route(&self.config.path, get(websocket_handler))
            .route("/health", get(health_handler))
            .with_state(state);
        let (shutdown_sender, shutdown_receiver) = oneshot::channel();
        let task = tokio::spawn(async move {
            let result = axum::serve(listener, app)
                .with_graceful_shutdown(async {
                    let _ = shutdown_receiver.await;
                })
                .await;
            result.map_err(|error| {
                LoomError::new(
                    ErrorCode::Internal,
                    format!("Loom WebSocket server stopped unexpectedly: {error}"),
                    true,
                )
            })
        });
        Ok(RunningRemoteServer {
            local_addr,
            websocket_url: self.config.websocket_url(local_addr),
            shutdown_sender: Some(shutdown_sender),
            task,
        })
    }
}

pub struct RunningRemoteServer {
    local_addr: SocketAddr,
    websocket_url: String,
    shutdown_sender: Option<oneshot::Sender<()>>,
    task: JoinHandle<Result<()>>,
}

impl RunningRemoteServer {
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    pub fn websocket_url(&self) -> &str {
        &self.websocket_url
    }

    pub async fn stop(mut self) -> Result<()> {
        if let Some(sender) = self.shutdown_sender.take() {
            let _ = sender.send(());
        }
        self.task.await.map_err(|error| {
            LoomError::new(
                ErrorCode::Internal,
                format!("Loom WebSocket server task failed: {error}"),
                true,
            )
        })?
    }
}

#[derive(Clone)]
struct RemoteState {
    backend: Arc<InProcessBackend>,
    auth: Arc<AuthTokenStore>,
    config: RemoteServerConfig,
}

async fn health_handler() -> impl IntoResponse {
    (StatusCode::OK, "ok")
}

/// Subprotocol prefix used by browser clients that cannot set an
/// `Authorization` header during the WebSocket handshake. The token travels in
/// `Sec-WebSocket-Protocol`, never in the URL, so it cannot leak through
/// request logs, referrers, or the address bar.
const BEARER_SUBPROTOCOL_PREFIX: &str = "loom.bearer.";

async fn websocket_handler(
    mut upgrade: WebSocketUpgrade,
    headers: HeaderMap,
    State(state): State<RemoteState>,
) -> Response {
    let (token, subprotocol) = match request_auth(&headers) {
        Ok(auth) => auth,
        Err(error) => return auth_error_response(error),
    };
    let auth = match state.auth.authenticate(token) {
        Ok(auth) => auth,
        Err(error) => return auth_error_response(error),
    };
    if let Some(protocol) = subprotocol {
        // Echo the selected subprotocol so the browser accepts the upgrade.
        upgrade = upgrade.protocols([protocol.to_owned()]);
    }
    upgrade
        .on_upgrade(move |socket| handle_socket(socket, state, auth))
        .into_response()
}

/// Resolves the bearer token for an incoming WebSocket upgrade.
///
/// Native clients send an `Authorization: Bearer` header. Browser `WebSocket`
/// clients cannot set arbitrary handshake headers, so they carry the token in a
/// `loom.bearer.<token>` subprotocol. The header takes precedence when both are
/// present.
fn request_auth(headers: &HeaderMap) -> Result<(&str, Option<&str>)> {
    if let Ok(token) = bearer_token(headers) {
        return Ok((token, None));
    }
    if let Some(protocols) = headers
        .get(SEC_WEBSOCKET_PROTOCOL)
        .and_then(|value| value.to_str().ok())
    {
        for protocol in protocols.split(',') {
            let protocol = protocol.trim();
            if let Some(token) = protocol.strip_prefix(BEARER_SUBPROTOCOL_PREFIX)
                && !token.is_empty()
            {
                return Ok((token, Some(protocol)));
            }
        }
    }
    Err(LoomError::new(
        ErrorCode::AuthenticationRequired,
        "authorization is required",
        false,
    ))
}

fn bearer_token(headers: &HeaderMap) -> Result<&str> {
    let value = headers
        .get(AUTHORIZATION)
        .ok_or_else(|| {
            LoomError::new(
                ErrorCode::AuthenticationRequired,
                "authorization header is required",
                false,
            )
        })?
        .to_str()
        .map_err(|_| {
            LoomError::new(
                ErrorCode::AuthenticationFailed,
                "authorization header is malformed",
                false,
            )
        })?;
    value.strip_prefix("Bearer ").ok_or_else(|| {
        LoomError::new(
            ErrorCode::AuthenticationFailed,
            "authorization must use the Bearer scheme",
            false,
        )
    })
}

fn auth_error_response(error: LoomError) -> Response {
    let status = match error.code {
        ErrorCode::AuthenticationRequired | ErrorCode::AuthenticationFailed => {
            StatusCode::UNAUTHORIZED
        }
        _ => StatusCode::FORBIDDEN,
    };
    (
        status,
        status.canonical_reason().unwrap_or("request rejected"),
    )
        .into_response()
}

enum ConnectionControl {
    Backpressure,
}

async fn handle_socket(socket: WebSocket, state: RemoteState, auth: AuthSession) {
    let connection = state.backend.connect_authenticated(auth);
    let (mut sender, mut receiver) = socket.split();
    let (control_sender, mut control_receiver) = mpsc::channel::<ConnectionControl>(1);
    let (message_sender, mut message_receiver) =
        mpsc::channel::<Message>(state.config.outgoing_capacity);
    let writer = tokio::spawn(async move {
        while let Some(message) = message_receiver.recv().await {
            if sender.send(message).await.is_err() {
                break;
            }
        }
    });

    let mut pending = HashMap::<RequestId, JoinHandle<()>>::new();
    let mut heartbeat = interval(state.config.heartbeat_interval);
    let mut last_activity = Instant::now();
    let heartbeat_timeout = state
        .config
        .heartbeat_interval
        .checked_mul(3)
        .unwrap_or(state.config.heartbeat_interval);

    loop {
        tokio::select! {
            control_message = control_receiver.recv() => {
                if matches!(control_message, Some(ConnectionControl::Backpressure)) {
                    break;
                }
            }
            _ = heartbeat.tick() => {
                if last_activity.elapsed() > heartbeat_timeout {
                    break;
                }
                if message_sender.try_send(Message::Ping(Vec::new().into())).is_err() {
                    break;
                }
            }
            incoming = receiver.next() => {
                let Some(incoming) = incoming else {
                    break;
                };
                match incoming {
                    Ok(Message::Text(text)) => {
                        last_activity = Instant::now();
                        process_text(
                            text.as_bytes(),
                            &connection,
                            &message_sender,
                            &control_sender,
                            &mut pending,
                            state.config.max_frame_bytes,
                            state.config.request_timeout,
                        ).await;
                    }
                    Ok(Message::Binary(bytes)) => {
                        last_activity = Instant::now();
                        process_text(
                            &bytes,
                            &connection,
                            &message_sender,
                            &control_sender,
                            &mut pending,
                            state.config.max_frame_bytes,
                            state.config.request_timeout,
                        ).await;
                    }
                    Ok(Message::Ping(payload)) => {
                        last_activity = Instant::now();
                        if message_sender.try_send(Message::Pong(payload)).is_err() {
                            break;
                        }
                    }
                    Ok(Message::Pong(_)) => {
                        last_activity = Instant::now();
                    }
                    Ok(Message::Close(_)) | Err(_) => break,
                }
            }
        }
    }
    for (_, task) in pending {
        task.abort();
    }
    drop(message_sender);
    let _ = writer.await;
}

async fn process_text(
    bytes: &[u8],
    connection: &InProcessConnection,
    outgoing: &mpsc::Sender<Message>,
    control: &mpsc::Sender<ConnectionControl>,
    pending: &mut HashMap<RequestId, JoinHandle<()>>,
    max_frame_bytes: usize,
    request_timeout: Duration,
) {
    if bytes.len() > max_frame_bytes {
        let response = ResponseEnvelope::failure(
            RequestId::new(),
            LoomError::new(
                ErrorCode::MalformedPayload,
                "WebSocket frame exceeds the configured size limit",
                false,
            ),
        );
        let _ = try_send_response(outgoing, response, control);
        return;
    }
    let request = match decode_request(bytes) {
        Ok(request) => request,
        Err(_) => match decode_client_frame(bytes) {
            Ok(ClientFrame::Request(request)) => *request,
            Ok(ClientFrame::Cancel { request_id }) => {
                if let Some(task) = pending.remove(&request_id) {
                    task.abort();
                }
                let response = ResponseEnvelope::failure(
                    request_id,
                    LoomError::new(
                        ErrorCode::RequestCancelled,
                        "request was cancelled by the client",
                        false,
                    ),
                );
                let _ = try_send_response(outgoing, response, control);
                return;
            }
            Err(_) => {
                let response = ResponseEnvelope::failure(
                    RequestId::new(),
                    LoomError::malformed_payload("WebSocket payload is not a Loom request"),
                );
                let _ = try_send_response(outgoing, response, control);
                return;
            }
        },
    };
    let request_id = request.request_id;
    let connection = connection.clone();
    let outgoing = outgoing.clone();
    let control = control.clone();
    let task = tokio::spawn(async move {
        let response = execute_request(connection, request, request_timeout).await;
        let _ = try_send_response(&outgoing, response, &control);
    });
    pending.retain(|_, task| !task.is_finished());
    pending.insert(request_id, task);
}

async fn execute_request(
    connection: InProcessConnection,
    request: RequestEnvelope,
    request_timeout: Duration,
) -> ResponseEnvelope {
    let request_id = request.request_id;
    execute_request_with(request_id, request_timeout, move || {
        connection.request(request)
    })
    .await
}

async fn execute_request_with(
    request_id: RequestId,
    request_timeout: Duration,
    operation: impl FnOnce() -> ResponseEnvelope + Send + 'static,
) -> ResponseEnvelope {
    match timeout(request_timeout, spawn_blocking(operation)).await {
        Ok(Ok(response)) => response,
        Ok(Err(error)) => ResponseEnvelope::failure(
            request_id,
            LoomError::new(
                ErrorCode::Internal,
                format!("remote request worker failed: {error}"),
                true,
            ),
        ),
        Err(_) => ResponseEnvelope::failure(
            request_id,
            LoomError::new(
                ErrorCode::DeadlineExceeded,
                "remote request exceeded its deadline",
                true,
            ),
        ),
    }
}

fn try_send_response(
    outgoing: &mpsc::Sender<Message>,
    response: ResponseEnvelope,
    control: &mpsc::Sender<ConnectionControl>,
) -> Result<()> {
    let bytes = encode_response(&response).map_err(|error| {
        LoomError::new(
            ErrorCode::Internal,
            format!("could not encode remote response: {error}"),
            false,
        )
    })?;
    let message = Message::Text(
        String::from_utf8(bytes)
            .map_err(|error| {
                LoomError::new(
                    ErrorCode::Internal,
                    format!("remote response was not UTF-8: {error}"),
                    false,
                )
            })?
            .into(),
    );
    match outgoing.try_send(message) {
        Ok(()) => Ok(()),
        Err(mpsc::error::TrySendError::Full(_)) => {
            let _ = control.try_send(ConnectionControl::Backpressure);
            Err(LoomError::new(
                ErrorCode::Backpressure,
                "remote client fell behind the bounded outbound queue",
                true,
            ))
        }
        Err(mpsc::error::TrySendError::Closed(_)) => Err(LoomError::new(
            ErrorCode::RequestCancelled,
            "remote client disconnected",
            true,
        )),
    }
}

#[derive(Clone)]
pub struct WebSocketTransport {
    url: String,
    bearer_token: String,
    max_frame_bytes: usize,
}

impl fmt::Debug for WebSocketTransport {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("WebSocketTransport")
            .field("url", &self.url)
            .field("bearer_token", &"<redacted>")
            .field("max_frame_bytes", &self.max_frame_bytes)
            .finish()
    }
}

impl WebSocketTransport {
    pub fn new(url: impl Into<String>, bearer_token: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            bearer_token: bearer_token.into(),
            max_frame_bytes: DEFAULT_MAX_FRAME_BYTES,
        }
    }

    pub fn with_max_frame_bytes(mut self, max_frame_bytes: usize) -> Result<Self> {
        if max_frame_bytes == 0 {
            return Err(LoomError::invalid_request(
                "WebSocket frame size must be greater than zero",
            ));
        }
        self.max_frame_bytes = max_frame_bytes;
        Ok(self)
    }

    pub fn url(&self) -> &str {
        &self.url
    }

    pub async fn connect(&self) -> Result<WebSocketConnection> {
        let mut request = self.url.clone().into_client_request().map_err(|_| {
            LoomError::invalid_request(
                "invalid WebSocket URL; provide a valid ws:// or wss:// endpoint",
            )
        })?;
        let value = format!("Bearer {}", self.bearer_token)
            .parse()
            .map_err(|_| LoomError::invalid_request("bearer token contains invalid characters"))?;
        request.headers_mut().insert(AUTHORIZATION, value);
        let (socket, _) = connect_async(request)
            .await
            .map_err(websocket_connect_error)?;
        Ok(WebSocketConnection {
            socket,
            max_frame_bytes: self.max_frame_bytes,
            events: Vec::new(),
        })
    }
}

pub struct WebSocketConnection {
    socket: tokio_tungstenite::WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>,
    max_frame_bytes: usize,
    events: Vec<ServerEventEnvelope>,
}

impl WebSocketConnection {
    pub async fn request(&mut self, request: RequestEnvelope) -> Result<ResponseEnvelope> {
        let request_id = request.request_id;
        let bytes = loom_protocol::encode_request(&request).map_err(|error| {
            LoomError::new(
                ErrorCode::MalformedPayload,
                format!("could not encode remote request: {error}"),
                false,
            )
        })?;
        if bytes.len() > self.max_frame_bytes {
            return Err(LoomError::new(
                ErrorCode::MalformedPayload,
                "remote request exceeds the configured frame size",
                false,
            ));
        }
        self.socket
            .send(ClientMessage::Text(
                String::from_utf8(bytes)
                    .map_err(|error| {
                        LoomError::new(
                            ErrorCode::MalformedPayload,
                            format!("remote request was not UTF-8: {error}"),
                            false,
                        )
                    })?
                    .into(),
            ))
            .await
            .map_err(socket_error)?;
        loop {
            let Some(message) = self.socket.next().await else {
                return Err(LoomError::new(
                    ErrorCode::RequestCancelled,
                    "remote server disconnected",
                    true,
                ));
            };
            match message.map_err(socket_error)? {
                ClientMessage::Text(text) => {
                    if text.len() > self.max_frame_bytes {
                        return Err(LoomError::new(
                            ErrorCode::MalformedPayload,
                            "remote response exceeds the configured frame size",
                            false,
                        ));
                    }
                    if let Ok(response) = loom_protocol::decode_response(text.as_bytes()) {
                        if response.request_id == request_id {
                            return Ok(response);
                        }
                        continue;
                    }
                    if let Ok(event) = decode_event(text.as_bytes()) {
                        self.events.push(event);
                        continue;
                    }
                    return Err(LoomError::malformed_payload(
                        "remote text frame is not a Loom response or event",
                    ));
                }
                ClientMessage::Binary(bytes) => {
                    if bytes.len() > self.max_frame_bytes {
                        return Err(LoomError::new(
                            ErrorCode::MalformedPayload,
                            "remote response exceeds the configured frame size",
                            false,
                        ));
                    }
                    if let Ok(response) = loom_protocol::decode_response(&bytes) {
                        if response.request_id == request_id {
                            return Ok(response);
                        }
                        continue;
                    }
                    if let Ok(event) = decode_event(&bytes) {
                        self.events.push(event);
                        continue;
                    }
                    return Err(LoomError::malformed_payload(
                        "remote binary frame is not a Loom response or event",
                    ));
                }
                ClientMessage::Ping(payload) => {
                    self.socket
                        .send(ClientMessage::Pong(payload))
                        .await
                        .map_err(socket_error)?;
                }
                ClientMessage::Pong(_) => {}
                ClientMessage::Close(_) => {
                    return Err(LoomError::new(
                        ErrorCode::RequestCancelled,
                        "remote server closed the connection",
                        true,
                    ));
                }
                ClientMessage::Frame(_) => {}
            }
        }
    }

    pub fn take_events(&mut self) -> Vec<ServerEventEnvelope> {
        std::mem::take(&mut self.events)
    }

    pub async fn cancel_request(&mut self, request_id: RequestId) -> Result<()> {
        let bytes = encode_client_frame(&ClientFrame::Cancel { request_id }).map_err(|error| {
            LoomError::new(
                ErrorCode::MalformedPayload,
                format!("could not encode remote cancellation: {error}"),
                false,
            )
        })?;
        self.socket
            .send(ClientMessage::Text(
                String::from_utf8(bytes)
                    .map_err(|error| {
                        LoomError::new(
                            ErrorCode::MalformedPayload,
                            format!("remote cancellation was not UTF-8: {error}"),
                            false,
                        )
                    })?
                    .into(),
            ))
            .await
            .map_err(socket_error)
    }

    pub async fn next_event(&mut self) -> Result<Option<ServerEventEnvelope>> {
        if !self.events.is_empty() {
            return Ok(Some(self.events.remove(0)));
        }
        loop {
            let Some(message) = self.socket.next().await else {
                return Ok(None);
            };
            match message.map_err(socket_error)? {
                ClientMessage::Text(text) => {
                    if let Ok(event) = decode_event(text.as_bytes()) {
                        return Ok(Some(event));
                    }
                    return Err(LoomError::malformed_payload(
                        "remote text frame is not a Loom event",
                    ));
                }
                ClientMessage::Binary(bytes) => {
                    if let Ok(event) = decode_event(&bytes) {
                        return Ok(Some(event));
                    }
                    return Err(LoomError::malformed_payload(
                        "remote binary frame is not a Loom event",
                    ));
                }
                ClientMessage::Ping(payload) => {
                    self.socket
                        .send(ClientMessage::Pong(payload))
                        .await
                        .map_err(socket_error)?;
                }
                ClientMessage::Pong(_) => {}
                ClientMessage::Close(_) => return Ok(None),
                ClientMessage::Frame(_) => {}
            }
        }
    }

    pub async fn close(&mut self) -> Result<()> {
        self.socket
            .send(ClientMessage::Close(None))
            .await
            .map_err(socket_error)
    }
}

fn socket_error(error: impl std::fmt::Display) -> LoomError {
    LoomError::new(
        ErrorCode::RequestCancelled,
        format!("WebSocket transport failed: {error}"),
        true,
    )
}

fn websocket_connect_error(error: TungsteniteError) -> LoomError {
    if matches!(
        error,
        TungsteniteError::Http(ref response)
            if response.status().as_u16() == StatusCode::UNAUTHORIZED.as_u16()
                || response.status().as_u16() == StatusCode::FORBIDDEN.as_u16()
    ) {
        LoomError::new(
            ErrorCode::AuthenticationFailed,
            "WebSocket authentication was rejected",
            false,
        )
    } else if error.to_string().to_ascii_lowercase().contains("timed out") {
        LoomError::new(
            ErrorCode::DeadlineExceeded,
            "worker connection timed out",
            true,
        )
    } else if error
        .to_string()
        .to_ascii_lowercase()
        .contains("connection refused")
    {
        LoomError::new(
            ErrorCode::ProviderUnavailable,
            "connection refused by remote worker",
            true,
        )
    } else {
        LoomError::new(
            ErrorCode::Internal,
            "could not open the worker WebSocket; check the URL and network access",
            true,
        )
    }
}

#[cfg(test)]
mod unit_tests {
    use super::{
        ConnectionControl, RemoteServer, RemoteServerConfig, WebSocketTransport,
        auth_error_response, bearer_token, execute_request_with, process_text, request_auth,
        try_send_response, websocket_connect_error,
    };
    use axum::{
        extract::ws::Message,
        http::{
            HeaderMap, HeaderValue,
            header::{AUTHORIZATION, SEC_WEBSOCKET_PROTOCOL},
        },
        response::IntoResponse,
    };
    use loom_core::{ErrorCode, ProtocolVersion, RequestId};
    use loom_protocol::{
        ClientFrame, ClientRequest, ControlRequest, ControlResponse, RequestEnvelope,
        ResponseEnvelope, ServerResponse, decode_response, encode_client_frame, encode_request,
    };
    use std::{
        collections::HashMap,
        io,
        net::{IpAddr, Ipv6Addr, SocketAddr},
        time::Duration,
    };
    use tokio::sync::mpsc;
    use tokio_tungstenite::tungstenite::Error as TungsteniteError;

    use super::super::AuthorizationScope;
    use super::{AuthTokenStore, InProcessBackend};
    use tokio_tungstenite::{
        connect_async,
        tungstenite::{Error as TungsteniteClientError, http::StatusCode},
    };

    #[tokio::test]
    async fn remote_websocket_rejects_unauthenticated_upgrade_and_protocol_9() {
        let backend = InProcessBackend::new();
        let auth = std::sync::Arc::new(AuthTokenStore::new());
        let token = auth.issue(AuthorizationScope::all()).unwrap();
        let server = RemoteServer::new(backend, auth, RemoteServerConfig::local_ephemeral())
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

        for field in 0..4 {
            let mut config = RemoteServerConfig::local_ephemeral();
            match field {
                0 => config.heartbeat_interval = Duration::ZERO,
                1 => config.request_timeout = Duration::ZERO,
                2 => config.max_frame_bytes = 0,
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

        process_text(
            b"too large",
            &connection,
            &outgoing,
            &control,
            &mut pending,
            1,
            Duration::from_secs(1),
        )
        .await;
        let Some(axum::extract::ws::Message::Text(text)) = received.recv().await else {
            panic!("expected oversized-frame response")
        };
        assert_eq!(
            decode_response(text.as_bytes())
                .unwrap()
                .result
                .unwrap_err()
                .code,
            ErrorCode::MalformedPayload
        );

        process_text(
            b"invalid",
            &connection,
            &outgoing,
            &control,
            &mut pending,
            100,
            Duration::from_secs(1),
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
            100,
            Duration::from_secs(1),
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
            1024,
            Duration::from_secs(1),
        )
        .await;
        let Some(axum::extract::ws::Message::Text(text)) = received.recv().await else {
            panic!("expected wrapped request response")
        };
        assert!(decode_response(text.as_bytes()).is_ok());
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
            1024,
            Duration::from_secs(1),
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
            1024,
            Duration::from_secs(1),
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
}
