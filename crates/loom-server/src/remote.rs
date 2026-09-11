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
        Query, State,
        ws::{Message, WebSocket, WebSocketUpgrade},
    },
    http::{HeaderMap, StatusCode, header::AUTHORIZATION},
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

async fn websocket_handler(
    upgrade: WebSocketUpgrade,
    headers: HeaderMap,
    Query(query): Query<HashMap<String, String>>,
    State(state): State<RemoteState>,
) -> Response {
    let auth = match request_token(&headers, &query)
        .and_then(|token| state.auth.authenticate(token))
    {
        Ok(auth) => auth,
        Err(error) => return auth_error_response(error),
    };
    upgrade
        .on_upgrade(move |socket| handle_socket(socket, state, auth))
        .into_response()
}

/// Resolves the bearer token for an incoming WebSocket upgrade.
///
/// Browser `WebSocket` clients cannot set arbitrary request headers during
/// the handshake, so in-browser clients authenticate with an `access_token`
/// query parameter instead of the `Authorization` header native clients use.
/// The header takes precedence when both are present.
fn request_token<'a>(
    headers: &'a HeaderMap,
    query: &'a HashMap<String, String>,
) -> Result<&'a str> {
    match bearer_token(headers) {
        Ok(token) => Ok(token),
        Err(header_error) => query
            .get("access_token")
            .map(String::as_str)
            .filter(|token| !token.is_empty())
            .ok_or(header_error),
    }
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
    match timeout(
        request_timeout,
        spawn_blocking(move || connection.request(request)),
    )
    .await
    {
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
        let mut request = self.url.clone().into_client_request().map_err(|error| {
            LoomError::invalid_request(format!("invalid WebSocket URL: {error}"))
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
    } else {
        LoomError::new(
            ErrorCode::Internal,
            format!("could not connect to Loom WebSocket service: {error}"),
            true,
        )
    }
}
