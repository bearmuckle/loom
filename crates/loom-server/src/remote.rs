use std::{
    collections::{HashMap, VecDeque},
    fmt, fs, io,
    net::{IpAddr, SocketAddr},
    path::Path,
    sync::Arc,
    time::Duration,
};

use axum::{
    Router,
    extract::{
        State,
        ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade},
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
    ClientFrame, ClientRequest, ProjectRequest, RequestEnvelope, ResponseEnvelope, RunRequest,
    ServerEventEnvelope, TaskRequest, TerminalRequest, decode_client_frame, decode_event,
    decode_request, encode_client_frame, encode_response,
};
use tokio::{
    net::{TcpListener, TcpStream},
    sync::{OwnedSemaphorePermit, Semaphore, mpsc, oneshot},
    task::{JoinHandle, spawn_blocking},
    time::{Instant, interval, timeout},
};
use tokio_rustls::TlsAcceptor;
use tokio_tungstenite::{
    Connector, MaybeTlsStream, connect_async_tls_with_config,
    tungstenite::{Error as TungsteniteError, Message as ClientMessage, client::IntoClientRequest},
};

use super::{AuthSession, AuthTokenStore, InProcessBackend, InProcessConnection};

const DEFAULT_WS_PATH: &str = "/ws";
const DEFAULT_HEARTBEAT: Duration = Duration::from_secs(15);
const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
/// The largest single WebSocket frame either side accepts, in bytes (4 MiB).
///
/// The protocol has no chunking, so a result that grows with workspace or
/// content size can cross this bound. Both sides then report
/// [`ErrorCode::PayloadTooLarge`] naming the frame size, the configured limit,
/// and the request, instead of implying that the frame was malformed. Raise the
/// bound with [`MAX_FRAME_BYTES_FLAG`] or [`MAX_FRAME_BYTES_ENV`] on a server and
/// on the clients that reach it, or per connection with
/// [`WebSocketTransport::with_max_frame_bytes`].
pub const DEFAULT_MAX_FRAME_BYTES: usize = 4 * 1024 * 1024;
/// The flag that carries the frame-size limit, named in parse errors.
pub const MAX_FRAME_BYTES_FLAG: &str = "--max-frame-bytes";
/// Environment variable that sets the frame-size limit when the flag is absent.
///
/// A server start and a native client resolve their limit from the same input,
/// so one value bounds both sides of the remote transport.
pub const MAX_FRAME_BYTES_ENV: &str = "LOOM_MAX_FRAME_BYTES";
/// WebSocket close code 1009, "message too big".
///
/// An oversized frame that names no request is closed with this code and a
/// reason carrying the same size-and-limit explanation, because there is no
/// request id to answer under.
const CLOSE_MESSAGE_TOO_BIG: u16 = 1009;
const DEFAULT_OUTGOING_CAPACITY: usize = 128;
/// Most ordinary requests one connection may have in flight at once.
///
/// Every received frame runs on a blocking thread, and Tokio's blocking queue
/// is unbounded, so without an explicit bound a client that pipelines frames
/// can queue unlimited blocking work and occupy the pool that other clients'
/// control and interrupt requests need. The frame that finds the limit reached
/// is refused with [`ErrorCode::Backpressure`] naming the limit; see
/// [`InFlightLimits`]. Set the limit with
/// [`RemoteServerConfig::max_in_flight_requests`].
pub const DEFAULT_MAX_IN_FLIGHT_REQUESTS: usize = 8;
/// Most control and interrupt requests one connection may have in flight at
/// once, counted separately from [`DEFAULT_MAX_IN_FLIGHT_REQUESTS`].
///
/// These requests are how a client observes a worker and stops work that is
/// already running, so they draw on their own permits: pipelined ordinary
/// requests cannot refuse or delay them. Set the limit with
/// [`RemoteServerConfig::max_in_flight_control_requests`]. See
/// [`is_control_or_interrupt_request`].
pub const DEFAULT_MAX_IN_FLIGHT_CONTROL_REQUESTS: usize = 4;
/// How long an incoming TLS handshake may take before the listener drops it.
const TLS_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
/// How long the client waits for the next frame while it is waiting for a
/// response.
///
/// The server heartbeats every [`DEFAULT_HEARTBEAT`] and answers every request
/// within its own configured request timeout, so a longer silence means the
/// socket is half-open rather than merely busy. Without this bound a stalled
/// socket left the calling thread waiting forever.
const CLIENT_FRAME_DEADLINE: Duration = Duration::from_secs(30);
/// Most events buffered while a request is in flight.
///
/// Events that arrive while a request is being answered are buffered for the
/// event stream, so the buffer is capped instead of growing with a session that
/// streams faster than the caller drains it.
const CLIENT_EVENT_BUFFER_CAPACITY: usize = 1024;

/// Whether this build's WebSocket transport can negotiate TLS (`wss://`).
///
/// The native transport enables `tokio-tungstenite`'s rustls features, so a
/// `wss://` endpoint is genuinely encrypted and can be verified against the OS
/// trust roots plus an operator-supplied CA. Clients report this capability
/// instead of trusting the URL scheme, so a build that cannot do TLS never
/// claims a `wss://` endpoint is secure.
pub const WEBSOCKET_TLS_SUPPORTED: bool = true;

/// Parses a frame-size limit in bytes.
///
/// `source` names the flag or environment variable the value came from, so an
/// operator is told which input to fix.
pub fn parse_max_frame_bytes(value: &str, source: &str) -> Result<usize> {
    let bytes = value.trim().parse::<usize>().map_err(|_| {
        LoomError::invalid_request(format!(
            "{source} must be a whole number of bytes such as 4194304; got '{value}'"
        ))
    })?;
    if bytes == 0 {
        return Err(LoomError::invalid_request(format!(
            "{source} must be greater than zero"
        )));
    }
    Ok(bytes)
}

/// Resolves the frame-size limit from an explicit flag and its environment
/// fallback, defaulting to [`DEFAULT_MAX_FRAME_BYTES`].
///
/// An explicit flag always wins over the variable it falls back to, exactly
/// like the archive-retention inputs. The standalone server, `loom --serve`, and
/// the native client all resolve their limit here, so the flag and the variable
/// mean the same thing on either side of the transport.
pub fn resolve_max_frame_bytes(flag: Option<&str>, env: Option<&str>) -> Result<usize> {
    match (flag, env) {
        (Some(value), _) => parse_max_frame_bytes(value, MAX_FRAME_BYTES_FLAG),
        (None, Some(value)) => parse_max_frame_bytes(value, MAX_FRAME_BYTES_ENV),
        (None, None) => Ok(DEFAULT_MAX_FRAME_BYTES),
    }
}

/// The diagnostic for a frame above the configured limit.
///
/// It names what the frame was, the request it belongs to, the received size,
/// and the configured limit, because the failure is a size bound rather than
/// corruption and the caller cannot otherwise tell which result to narrow.
fn frame_too_large_error(
    context: &str,
    request_id: RequestId,
    received: usize,
    limit: usize,
) -> LoomError {
    LoomError::payload_too_large(format!(
        "{context} of {received} bytes for request {request_id} is above the configured frame \
         size limit of {limit} bytes"
    ))
}

/// The close reason for an oversized frame that names no request.
///
/// A WebSocket close reason is limited to 123 bytes, so this carries the two
/// numbers and nothing else.
fn frame_too_large_close_reason(received: usize, limit: usize) -> String {
    format!("frame of {received} bytes over the configured {limit}-byte limit")
}

/// Decides which request an inbound frame above the configured limit fails.
///
/// A frame that decodes as the response the caller awaits belongs to that
/// request, so only that request fails. Anything else cannot be that answer, so
/// it is drained and the wait continues; see
/// [`WebSocketConnection::oversized_inbound_frame`] for the rule and its
/// reasoning.
fn oversized_inbound_frame_action(frame: &[u8], request_id: RequestId) -> OversizedFrameAction {
    match loom_protocol::decode_response(frame) {
        Ok(response) if response.request_id == request_id => {
            OversizedFrameAction::FailAwaitedRequest
        }
        Ok(_) | Err(_) => OversizedFrameAction::Drain,
    }
}

/// How an over-limit inbound frame is treated while a request is awaited.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum OversizedFrameAction {
    /// Fail the awaited request with the payload-too-large diagnostic.
    FailAwaitedRequest,
    /// Drop the frame and keep waiting for the awaited response.
    Drain,
}

/// Server-side TLS material for the remote WebSocket listener.
///
/// Both PEM documents are needed to serve `wss://`: the certificate chain
/// presented to clients and the private key that matches its leaf.
#[derive(Clone)]
pub struct ServerTlsConfig {
    certificate_pem: Vec<u8>,
    private_key_pem: Vec<u8>,
}

impl fmt::Debug for ServerTlsConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ServerTlsConfig")
            .field("certificate_pem_bytes", &self.certificate_pem.len())
            .field("private_key_pem", &"<redacted>")
            .finish()
    }
}

impl ServerTlsConfig {
    /// Builds TLS material from in-memory PEM documents.
    pub fn from_pem(
        certificate_pem: impl Into<Vec<u8>>,
        private_key_pem: impl Into<Vec<u8>>,
    ) -> Self {
        Self {
            certificate_pem: certificate_pem.into(),
            private_key_pem: private_key_pem.into(),
        }
    }

    /// Reads the certificate chain and private key from PEM files.
    pub fn from_pem_files(
        certificate: impl AsRef<Path>,
        private_key: impl AsRef<Path>,
    ) -> Result<Self> {
        let certificate = certificate.as_ref();
        let private_key = private_key.as_ref();
        let certificate_pem = fs::read(certificate).map_err(|error| {
            LoomError::invalid_request(format!(
                "could not read the TLS certificate file '{}': {error}",
                certificate.display()
            ))
        })?;
        let private_key_pem = fs::read(private_key).map_err(|error| {
            LoomError::invalid_request(format!(
                "could not read the TLS private key file '{}': {error}",
                private_key.display()
            ))
        })?;
        Ok(Self::from_pem(certificate_pem, private_key_pem))
    }

    /// Builds the rustls server configuration this listener serves with.
    fn server_config(&self) -> Result<Arc<rustls::ServerConfig>> {
        let mut certificate_reader = io::BufReader::new(self.certificate_pem.as_slice());
        let certificates = rustls_pemfile::certs(&mut certificate_reader)
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|error| {
                LoomError::invalid_request(format!(
                    "could not parse the TLS certificate PEM: {error}"
                ))
            })?;
        if certificates.is_empty() {
            return Err(LoomError::invalid_request(
                "the TLS certificate PEM contains no certificates",
            ));
        }
        let mut key_reader = io::BufReader::new(self.private_key_pem.as_slice());
        let private_key = rustls_pemfile::private_key(&mut key_reader)
            .map_err(|error| {
                LoomError::invalid_request(format!(
                    "could not parse the TLS private key PEM: {error}"
                ))
            })?
            .ok_or_else(|| {
                LoomError::invalid_request("the TLS private key PEM contains no private key")
            })?;
        let config = rustls::ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(certificates, private_key)
            .map_err(|error| {
                LoomError::invalid_request(format!(
                    "the TLS certificate and private key cannot be used together: {error}"
                ))
            })?;
        Ok(Arc::new(config))
    }
}

#[derive(Clone, Debug)]
pub struct RemoteServerConfig {
    pub bind_addr: SocketAddr,
    pub path: String,
    pub heartbeat_interval: Duration,
    pub request_timeout: Duration,
    /// Most bytes the listener accepts in one inbound WebSocket frame. It
    /// defaults to [`DEFAULT_MAX_FRAME_BYTES`] (4 MiB) and is raised with
    /// [`MAX_FRAME_BYTES_FLAG`] or [`MAX_FRAME_BYTES_ENV`].
    ///
    /// It is also the bound the listener applies to the responses it sends: one
    /// connection can never emit a frame above the limit it advertises, so an
    /// events batch larger than this is delivered as bounded pages (see
    /// [`loom_protocol::EventsResponse::SessionEvents`]), and an event that
    /// cannot fit at all is reported as [`ErrorCode::PayloadTooLarge`] naming
    /// its sequence and size rather than sent as a frame the client drops.
    pub max_frame_bytes: usize,
    /// Most ordinary requests one connection may have in flight at once.
    ///
    /// A frame that arrives while the connection already has this many
    /// ordinary requests running is answered with [`ErrorCode::Backpressure`]
    /// naming the limit, instead of queueing an unbounded amount of blocking
    /// work. It defaults to [`DEFAULT_MAX_IN_FLIGHT_REQUESTS`].
    pub max_in_flight_requests: usize,
    /// Most control and interrupt requests one connection may have in flight
    /// at once.
    ///
    /// The permits are counted separately from
    /// [`Self::max_in_flight_requests`], so a client that pipelines ordinary
    /// requests cannot refuse or delay its own capability discovery,
    /// negotiation, worker status, run interrupts, or child and terminal
    /// cancellation. It defaults to
    /// [`DEFAULT_MAX_IN_FLIGHT_CONTROL_REQUESTS`].
    pub max_in_flight_control_requests: usize,
    pub outgoing_capacity: usize,
    /// TLS material for the listener. `Some` serves `wss://` and `/health` over
    /// TLS; `None` serves plain `ws://` and is only allowed on loopback unless
    /// [`RemoteServerConfig::allow_insecure_remote`] is set.
    pub tls: Option<ServerTlsConfig>,
    /// Explicit opt-in that permits a plaintext listener beyond loopback. The
    /// bearer token is sent in the clear, so the operator has to ask for it.
    pub allow_insecure_remote: bool,
}

impl Default for RemoteServerConfig {
    fn default() -> Self {
        Self {
            bind_addr: SocketAddr::new(IpAddr::V4(std::net::Ipv4Addr::LOCALHOST), 8765),
            path: DEFAULT_WS_PATH.to_owned(),
            heartbeat_interval: DEFAULT_HEARTBEAT,
            request_timeout: DEFAULT_REQUEST_TIMEOUT,
            max_frame_bytes: DEFAULT_MAX_FRAME_BYTES,
            max_in_flight_requests: DEFAULT_MAX_IN_FLIGHT_REQUESTS,
            max_in_flight_control_requests: DEFAULT_MAX_IN_FLIGHT_CONTROL_REQUESTS,
            outgoing_capacity: DEFAULT_OUTGOING_CAPACITY,
            tls: None,
            allow_insecure_remote: false,
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
        let scheme = if self.tls.is_some() { "wss" } else { "ws" };
        format!("{scheme}://{host}:{}{}", address.port(), self.path)
    }

    /// Whether this configuration serves the listener over TLS.
    pub fn tls_enabled(&self) -> bool {
        self.tls.is_some()
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
            || self.config.max_in_flight_requests == 0
            || self.config.max_in_flight_control_requests == 0
            || self.config.outgoing_capacity == 0
        {
            return Err(LoomError::invalid_request(
                "remote server limits must be greater than zero",
            ));
        }
        self.check_exposure()?;
        let tls_config = match &self.config.tls {
            Some(tls) => Some(tls.server_config()?),
            None => None,
        };
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
        let shutdown = async move {
            let _ = shutdown_receiver.await;
        };
        let task = match tls_config {
            Some(tls_config) => {
                let listener = TlsListener::new(listener, TlsAcceptor::from(tls_config));
                tokio::spawn(async move {
                    axum::serve(listener, app)
                        .with_graceful_shutdown(shutdown)
                        .await
                        .map_err(server_stopped_error)
                })
            }
            None => tokio::spawn(async move {
                axum::serve(listener, app)
                    .with_graceful_shutdown(shutdown)
                    .await
                    .map_err(server_stopped_error)
            }),
        };
        Ok(RunningRemoteServer {
            local_addr,
            websocket_url: self.config.websocket_url(local_addr),
            tls_enabled: self.config.tls_enabled(),
            shutdown_sender: Some(shutdown_sender),
            task,
        })
    }

    /// Refuses to publish plaintext `ws://` traffic beyond loopback unless the
    /// caller configured TLS or explicitly opted in.
    fn check_exposure(&self) -> Result<()> {
        let bind_addr = self.config.bind_addr;
        if bind_addr.ip().is_loopback() || self.config.tls.is_some() {
            return Ok(());
        }
        if !self.config.allow_insecure_remote {
            return Err(LoomError::invalid_request(format!(
                "refusing to bind {bind_addr}: the remote WebSocket transport carries a bearer \
                 token and cannot protect it without TLS; configure a TLS certificate and key, \
                 bind a loopback address, or opt in to an insecure remote bind"
            )));
        }
        log::warn!(
            "binding {bind_addr} without TLS because insecure remote access was explicitly \
             allowed; every bearer token and protocol frame on this listener is sent in plaintext"
        );
        Ok(())
    }
}

fn server_stopped_error(error: io::Error) -> LoomError {
    LoomError::new(
        ErrorCode::Internal,
        format!("Loom WebSocket server stopped unexpectedly: {error}"),
        true,
    )
}

/// A listening socket that completes a TLS handshake with every accepted
/// connection, so `axum::serve` handles `wss://` and `/health` over TLS with the
/// same router as the plaintext listener.
struct TlsListener {
    listener: TcpListener,
    acceptor: TlsAcceptor,
}

impl TlsListener {
    fn new(listener: TcpListener, acceptor: TlsAcceptor) -> Self {
        Self { listener, acceptor }
    }
}

impl axum::serve::Listener for TlsListener {
    type Io = tokio_rustls::server::TlsStream<TcpStream>;
    type Addr = SocketAddr;

    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        loop {
            let (stream, address) = match self.listener.accept().await {
                Ok(accepted) => accepted,
                Err(error) => {
                    log::warn!("TLS listener could not accept a connection: {error}");
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    continue;
                }
            };
            // A client that never finishes the handshake must not hold the
            // accept loop, so cap how long the handshake may take.
            match timeout(TLS_HANDSHAKE_TIMEOUT, self.acceptor.accept(stream)).await {
                Ok(Ok(stream)) => return (stream, address),
                Ok(Err(error)) => {
                    log::warn!("TLS handshake with {address} failed: {error}");
                }
                Err(_) => {
                    log::warn!("TLS handshake with {address} timed out");
                }
            }
        }
    }

    fn local_addr(&self) -> io::Result<Self::Addr> {
        self.listener.local_addr()
    }
}

pub struct RunningRemoteServer {
    local_addr: SocketAddr,
    websocket_url: String,
    tls_enabled: bool,
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

    /// Whether this listener serves TLS.
    pub fn tls_enabled(&self) -> bool {
        self.tls_enabled
    }

    /// The unauthenticated health endpoint, using the scheme the listener
    /// actually serves.
    pub fn health_url(&self) -> String {
        let scheme = if self.tls_enabled { "https" } else { "http" };
        format!("{scheme}://{}/health", self.local_addr)
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

/// Which in-flight permit pool a request draws on.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RequestClass {
    /// A request that does work but is not how a client observes or stops it.
    Ordinary,
    /// A request that tells a client what the worker can do or stops work that
    /// is already running; see [`is_control_or_interrupt_request`].
    Control,
}

impl RequestClass {
    fn of(request: &ClientRequest) -> Self {
        if is_control_or_interrupt_request(request) {
            Self::Control
        } else {
            Self::Ordinary
        }
    }
}

/// Whether a request is a control or interrupt request.
///
/// These are the requests that report what a worker can do (capability
/// discovery and negotiation, worker status) and that stop work already running
/// (run interrupts and action approvals, project-child control, task and
/// terminal cancellation). They must not be refused or delayed because the
/// connection is busy with pipelined ordinary requests, so they draw on their
/// own in-flight permits; see [`InFlightLimits`].
fn is_control_or_interrupt_request(request: &ClientRequest) -> bool {
    matches!(
        request,
        ClientRequest::Control(_)
            | ClientRequest::Run(RunRequest::InterruptAgentRun { .. })
            | ClientRequest::Run(RunRequest::ApproveAgentAction { .. })
            | ClientRequest::Run(RunRequest::RejectAgentAction { .. })
            | ClientRequest::Project(ProjectRequest::ControlProjectChild { .. })
            | ClientRequest::Task(TaskRequest::CancelSessionTask { .. })
            | ClientRequest::Terminal(TerminalRequest::CancelSessionTerminal { .. })
    )
}

/// The in-flight request permits of one connection, one pool per request class.
///
/// Every received frame runs its request on a blocking thread, and Tokio's
/// blocking queue is unbounded, so the pools are what bounds how much blocking
/// work one client can queue: a frame that finds its pool exhausted is refused
/// with [`ErrorCode::Backpressure`] naming the limit, and the permit is held by
/// the blocking operation itself, so aborting the waiting task does not hand
/// the slot back early.
struct InFlightLimits {
    ordinary: Arc<Semaphore>,
    control: Arc<Semaphore>,
    max_ordinary: usize,
    max_control: usize,
}

impl InFlightLimits {
    /// The limits come from [`RemoteServerConfig::max_in_flight_requests`] and
    /// [`RemoteServerConfig::max_in_flight_control_requests`], which
    /// [`RemoteServer::bind`] requires to be greater than zero.
    fn new(max_ordinary: usize, max_control: usize) -> Self {
        Self {
            ordinary: Arc::new(Semaphore::new(max_ordinary)),
            control: Arc::new(Semaphore::new(max_control)),
            max_ordinary,
            max_control,
        }
    }

    /// Reserves one in-flight slot for a request of `class`, or reports the
    /// refusal that answers it.
    ///
    /// The refusal names the limit and states that the request was not started,
    /// because a client whose request was dropped silently cannot tell a lost
    /// frame from a slow worker.
    fn admit(&self, request_id: RequestId, class: RequestClass) -> Result<OwnedSemaphorePermit> {
        let (permits, limit, field, kind) = match class {
            RequestClass::Ordinary => (
                &self.ordinary,
                self.max_ordinary,
                "max_in_flight_requests",
                "in-flight requests",
            ),
            RequestClass::Control => (
                &self.control,
                self.max_control,
                "max_in_flight_control_requests",
                "in-flight control requests",
            ),
        };
        Arc::clone(permits).try_acquire_owned().map_err(|_| {
            LoomError::new(
                ErrorCode::Backpressure,
                format!(
                    "request {request_id} was not started: this connection is at its configured \
                     limit of {limit} {kind} ({field}); wait for an outstanding response before \
                     sending another"
                ),
                true,
            )
        })
    }
}

/// The bounds one connection applies to every frame it receives.
///
/// The pools are per connection, so they are built once per accepted socket and
/// shared by every frame on it.
struct ConnectionLimits {
    /// Most bytes an inbound frame may carry; see [`DEFAULT_MAX_FRAME_BYTES`].
    max_frame_bytes: usize,
    /// How long a request may wait for its blocking operation.
    request_timeout: Duration,
    in_flight: InFlightLimits,
}

impl ConnectionLimits {
    fn new(config: &RemoteServerConfig) -> Self {
        Self {
            max_frame_bytes: config.max_frame_bytes,
            request_timeout: config.request_timeout,
            in_flight: InFlightLimits::new(
                config.max_in_flight_requests,
                config.max_in_flight_control_requests,
            ),
        }
    }
}

/// The error that answers a request whose waiting future was abandoned.
///
/// Cancellation and the request deadline stop only the *wait*: the operation
/// runs on a blocking thread, and Tokio cannot cancel a blocking closure, so the
/// mutation may still be applied and persisted after the caller is answered.
/// Reporting plain `RequestCancelled` or `DeadlineExceeded` would claim the
/// opposite, so the response says what actually holds: the request was
/// abandoned rather than undone, and its effect may still land. A caller that
/// retries with the same request id is answered from the idempotency cache once
/// the operation committed, so retrying is safe for durable mutations.
fn abandoned_request_error(
    code: ErrorCode,
    request_id: RequestId,
    reason: &str,
    retryable: bool,
) -> LoomError {
    LoomError::new(
        code,
        format!(
            "request {request_id} was abandoned {reason}; the operation runs on a blocking thread \
             that abandoning the wait does not stop, so it may still complete and its effect may \
             still be applied; read the state again before assuming it did not happen"
        ),
        retryable,
    )
}

async fn handle_socket(socket: WebSocket, state: RemoteState, auth: AuthSession) {
    // The connection carries the listener's frame limit, so an events batch it
    // answers with is bounded to a frame its clients accept instead of reaching
    // them as an over-limit frame that their own rule drains.
    let connection = state
        .backend
        .connect_authenticated(auth)
        .with_max_response_bytes(state.config.max_frame_bytes);
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
    let limits = ConnectionLimits::new(&state.config);
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
                        if let Some(close) = process_text(
                            text.as_bytes(),
                            &connection,
                            &message_sender,
                            &control_sender,
                            &mut pending,
                            &limits,
                        ).await {
                            // The reason is the only explanation an oversized
                            // frame that named no request can carry, so it is
                            // sent before the writer is dropped below.
                            let _ = message_sender.send(Message::Close(Some(close))).await;
                            break;
                        }
                    }
                    Ok(Message::Binary(bytes)) => {
                        last_activity = Instant::now();
                        if let Some(close) = process_text(
                            &bytes,
                            &connection,
                            &message_sender,
                            &control_sender,
                            &mut pending,
                            &limits,
                        ).await {
                            let _ = message_sender.send(Message::Close(Some(close))).await;
                            break;
                        }
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

/// Handles one inbound frame, returning the close frame that ends the
/// connection when one must be sent.
///
/// An oversized frame is rejected as [`ErrorCode::PayloadTooLarge`] rather than
/// as a malformed payload, because the bytes it rejected are well formed. When
/// the frame still decodes, the rejection is answered under the request id it
/// names, so the client sees the failure on the request it sent. A frame that
/// names no request cannot be answered that way, so the connection closes with
/// a reason carrying the same size-and-limit explanation.
///
/// A request that arrives while the connection already has its in-flight limit
/// of requests running is answered with [`ErrorCode::Backpressure`] naming the
/// limit, so the client learns its request was refused rather than started, and
/// the connection stays open. See [`InFlightLimits`].
async fn process_text(
    bytes: &[u8],
    connection: &InProcessConnection,
    outgoing: &mpsc::Sender<Message>,
    control: &mpsc::Sender<ConnectionControl>,
    pending: &mut HashMap<RequestId, JoinHandle<()>>,
    limits: &ConnectionLimits,
) -> Option<CloseFrame> {
    let max_frame_bytes = limits.max_frame_bytes;
    if bytes.len() > max_frame_bytes {
        let received = bytes.len();
        if let Some(request_id) = frame_request_id(bytes) {
            let response = ResponseEnvelope::failure(
                request_id,
                frame_too_large_error("WebSocket frame", request_id, received, max_frame_bytes),
            );
            let _ = try_send_response(outgoing, response, control);
            return None;
        }
        let reason = frame_too_large_close_reason(received, max_frame_bytes);
        log::warn!("closing remote connection: {reason}");
        return Some(CloseFrame {
            code: CLOSE_MESSAGE_TOO_BIG,
            reason: reason.into(),
        });
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
                    abandoned_request_error(
                        ErrorCode::RequestCancelled,
                        request_id,
                        "by the client",
                        false,
                    ),
                );
                let _ = try_send_response(outgoing, response, control);
                return None;
            }
            Err(_) => {
                let response = ResponseEnvelope::failure(
                    RequestId::new(),
                    LoomError::malformed_payload("WebSocket payload is not a Loom request"),
                );
                let _ = try_send_response(outgoing, response, control);
                return None;
            }
        },
    };
    let request_id = request.request_id;
    let class = RequestClass::of(&request.request);
    let permit = match limits.in_flight.admit(request_id, class) {
        Ok(permit) => permit,
        Err(error) => {
            // The refusal carries the request id, so the client sees on the
            // request it sent that it was refused instead of started.
            log::debug!(
                "refused remote request {request_id}: the connection is at its {class:?} \
                 in-flight limit"
            );
            let response = ResponseEnvelope::failure(request_id, error);
            let _ = try_send_response(outgoing, response, control);
            return None;
        }
    };
    let request_timeout = limits.request_timeout;
    let connection = connection.clone();
    let outgoing = outgoing.clone();
    let control = control.clone();
    let task = tokio::spawn(async move {
        let response = execute_request(connection, request, request_timeout, permit).await;
        let _ = try_send_response(&outgoing, response, &control);
    });
    pending.retain(|_, task| !task.is_finished());
    pending.insert(request_id, task);
    None
}

/// The request id an inbound frame names, when it can still be decoded.
///
/// The oversized branch uses this to correlate its rejection, so a frame that
/// names no request is never answered under a fabricated id.
fn frame_request_id(bytes: &[u8]) -> Option<RequestId> {
    if let Ok(request) = decode_request(bytes) {
        return Some(request.request_id);
    }
    match decode_client_frame(bytes) {
        Ok(ClientFrame::Request(request)) => Some(request.request_id),
        Ok(ClientFrame::Cancel { request_id }) => Some(request_id),
        Err(_) => None,
    }
}

async fn execute_request(
    connection: InProcessConnection,
    request: RequestEnvelope,
    request_timeout: Duration,
    permit: OwnedSemaphorePermit,
) -> ResponseEnvelope {
    let request_id = request.request_id;
    execute_request_with(request_id, request_timeout, move || {
        // The in-flight permit belongs to the blocking operation, not to the
        // task waiting for it: aborting that task (a cancel frame or the
        // deadline below) must not hand the slot back while the operation is
        // still running.
        let _permit = permit;
        connection.request(request)
    })
    .await
}

/// Runs `operation` on a blocking thread with `request_timeout` as its deadline.
///
/// The deadline stops the *wait*, not the operation: Tokio cannot cancel a
/// blocking closure, so an operation that outlives it keeps running and may
/// still apply and persist its mutation. The response says so rather than
/// reporting the request as expired while its effect lands; see
/// [`abandoned_request_error`].
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
            abandoned_request_error(
                ErrorCode::DeadlineExceeded,
                request_id,
                &format!("when its {request_timeout:?} request deadline passed"),
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
    /// Rustls configuration that trusts the OS roots plus an operator-supplied
    /// CA. `None` uses the default OS trust roots.
    tls_client_config: Option<Arc<rustls::ClientConfig>>,
    /// Explicit opt-in that permits plaintext `ws://` to a non-loopback host.
    allow_insecure_remote: bool,
}

impl fmt::Debug for WebSocketTransport {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("WebSocketTransport")
            .field("url", &self.url)
            .field("bearer_token", &"<redacted>")
            .field("max_frame_bytes", &self.max_frame_bytes)
            .field("ca_certificate", &self.tls_client_config.is_some())
            .field("allow_insecure_remote", &self.allow_insecure_remote)
            .finish()
    }
}

impl WebSocketTransport {
    pub fn new(url: impl Into<String>, bearer_token: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            bearer_token: bearer_token.into(),
            max_frame_bytes: DEFAULT_MAX_FRAME_BYTES,
            tls_client_config: None,
            allow_insecure_remote: false,
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

    /// Trusts the certificates in this PEM document in addition to the OS trust
    /// roots. The supplied CA is never a substitute for the OS roots, so an
    /// unrelated certificate cannot be accepted on its behalf.
    pub fn with_ca_certificate_pem(mut self, certificate_pem: impl AsRef<[u8]>) -> Result<Self> {
        self.tls_client_config = Some(client_tls_config(certificate_pem.as_ref())?);
        Ok(self)
    }

    /// Trusts the certificates in this PEM file in addition to the OS trust
    /// roots.
    pub fn with_ca_certificate_file(self, certificate: impl AsRef<Path>) -> Result<Self> {
        let certificate = certificate.as_ref();
        let pem = fs::read(certificate).map_err(|error| {
            LoomError::invalid_request(format!(
                "could not read the TLS CA file '{}': {error}",
                certificate.display()
            ))
        })?;
        self.with_ca_certificate_pem(pem)
    }

    /// Allows plaintext `ws://` to a host that is not the local machine. The
    /// bearer token and every protocol frame travel in the clear, so this must
    /// be an explicit operator decision; the connection warns when it is used.
    pub fn with_insecure_remote_allowed(mut self, allow_insecure_remote: bool) -> Self {
        self.allow_insecure_remote = allow_insecure_remote;
        self
    }

    pub fn url(&self) -> &str {
        &self.url
    }

    pub async fn connect(&self) -> Result<WebSocketConnection> {
        if let Some(host) = insecure_remote_host(&self.url) {
            if !self.allow_insecure_remote {
                return Err(LoomError::invalid_request(format!(
                    "refusing to send the worker access token over plaintext ws:// to '{host}'; \
                     use wss:// with a trusted certificate, connect to a loopback worker, or \
                     pass --allow-insecure-remote to accept an unencrypted connection"
                )));
            }
            log::warn!(
                "connecting to '{host}' over plaintext ws:// because insecure remote access was \
                 explicitly allowed; the worker access token and all protocol traffic are not \
                 protected in transit"
            );
        }
        let mut request = self.url.clone().into_client_request().map_err(|_| {
            LoomError::invalid_request(
                "invalid WebSocket URL; provide a valid ws:// or wss:// endpoint",
            )
        })?;
        let value = format!("Bearer {}", self.bearer_token)
            .parse()
            .map_err(|_| LoomError::invalid_request("bearer token contains invalid characters"))?;
        request.headers_mut().insert(AUTHORIZATION, value);
        let connector = self.tls_client_config.clone().map(Connector::Rustls);
        let (socket, _) = connect_async_tls_with_config(request, None, false, connector)
            .await
            .map_err(websocket_connect_error)?;
        Ok(WebSocketConnection {
            socket,
            max_frame_bytes: self.max_frame_bytes,
            events: VecDeque::new(),
        })
    }
}

pub struct WebSocketConnection {
    socket: tokio_tungstenite::WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>,
    max_frame_bytes: usize,
    /// Events read while a request was in flight, oldest first and bounded by
    /// [`CLIENT_EVENT_BUFFER_CAPACITY`].
    events: VecDeque<ServerEventEnvelope>,
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
            return Err(frame_too_large_error(
                "remote request",
                request_id,
                bytes.len(),
                self.max_frame_bytes,
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
            // A half-open socket never yields a frame, so each individual wait
            // is bounded instead of blocking the calling thread forever.
            let next = timeout(CLIENT_FRAME_DEADLINE, self.socket.next()).await;
            let message = match next {
                Ok(Some(message)) => message,
                Ok(None) => {
                    return Err(LoomError::new(
                        ErrorCode::RequestCancelled,
                        "remote server disconnected",
                        true,
                    ));
                }
                Err(_) => {
                    return Err(LoomError::new(
                        ErrorCode::DeadlineExceeded,
                        "remote server sent no frame within the read deadline; the connection may \
                         be stalled",
                        true,
                    ));
                }
            };
            match message.map_err(socket_error)? {
                ClientMessage::Text(text) => {
                    if text.len() > self.max_frame_bytes {
                        // The check runs before decoding, so it sees every frame
                        // on the socket, including events and other requests'
                        // responses. An over-limit frame therefore only fails
                        // the request it decodes as the response for; anything
                        // else is drained and the wait continues, so concurrent
                        // traffic cannot fail a request that did not cause it.
                        if let Some(error) =
                            self.oversized_inbound_frame(text.as_bytes(), request_id)
                        {
                            return Err(error);
                        }
                        continue;
                    }
                    if let Ok(response) = loom_protocol::decode_response(text.as_bytes()) {
                        if response.request_id == request_id {
                            return Ok(response);
                        }
                        continue;
                    }
                    if let Ok(event) = decode_event(text.as_bytes()) {
                        self.buffer_event(event);
                        continue;
                    }
                    return Err(LoomError::malformed_payload(
                        "remote text frame is not a Loom response or event",
                    ));
                }
                ClientMessage::Binary(bytes) => {
                    if bytes.len() > self.max_frame_bytes {
                        // Same rule as the text arm above.
                        if let Some(error) = self.oversized_inbound_frame(&bytes, request_id) {
                            return Err(error);
                        }
                        continue;
                    }
                    if let Ok(response) = loom_protocol::decode_response(&bytes) {
                        if response.request_id == request_id {
                            return Ok(response);
                        }
                        continue;
                    }
                    if let Ok(event) = decode_event(&bytes) {
                        self.buffer_event(event);
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

    /// Decides what an inbound frame above the configured limit does to the
    /// request being awaited.
    ///
    /// The size check runs before decoding, so it applies to every frame on the
    /// socket, including events and other requests' responses. Deciding by
    /// content keeps an over-limit frame from failing a request it has nothing
    /// to do with:
    ///
    /// - a frame that decodes as the awaited response fails exactly that
    ///   request with [`ErrorCode::PayloadTooLarge`], naming the received size,
    ///   the configured limit, and the request, because its answer can never be
    ///   delivered;
    /// - anything else — an event, another request's response, or a frame that
    ///   does not decode at all — is drained and the wait continues. A drained
    ///   event is dropped rather than buffered, because buffering it would
    ///   defeat the limit; events carry a sequence, so a caller that drains the
    ///   stream can detect the gap.
    ///
    /// Returns the error the awaited request fails with, or `None` when the
    /// frame was drained.
    fn oversized_inbound_frame(&self, frame: &[u8], request_id: RequestId) -> Option<LoomError> {
        match oversized_inbound_frame_action(frame, request_id) {
            OversizedFrameAction::FailAwaitedRequest => Some(frame_too_large_error(
                "remote response",
                request_id,
                frame.len(),
                self.max_frame_bytes,
            )),
            OversizedFrameAction::Drain => {
                log::warn!(
                    "dropped a remote frame of {} bytes while waiting for request {request_id}: it \
                     is above the configured frame size limit of {} bytes and is not that \
                     request's response",
                    frame.len(),
                    self.max_frame_bytes
                );
                None
            }
        }
    }

    /// Buffers an event read while a request was in flight.
    ///
    /// The buffer is capped: when a session streams faster than the caller
    /// drains it, the oldest events are dropped so the process cannot grow
    /// without limit. Events carry a sequence, so a resuming caller can detect
    /// the gap and re-sync instead of silently skipping it.
    fn buffer_event(&mut self, event: ServerEventEnvelope) {
        let capacity = CLIENT_EVENT_BUFFER_CAPACITY;
        if self.events.len() >= capacity {
            let dropped = self.events.len() - capacity / 2;
            self.events.drain(..dropped);
            log::warn!(
                "dropped {dropped} buffered remote events: more than {capacity} events arrived \
                 while a request was in flight"
            );
        }
        self.events.push_back(event);
    }

    pub fn take_events(&mut self) -> Vec<ServerEventEnvelope> {
        self.events.drain(..).collect()
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
        if let Some(event) = self.events.pop_front() {
            return Ok(Some(event));
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

/// Builds a rustls client configuration that trusts the OS roots and, in
/// addition, every certificate in `ca_pem`.
///
/// The supplied CA is added to the OS roots rather than replacing them, so
/// trusting a private CA never widens trust to unrelated certificates that the
/// OS already rejects.
fn client_tls_config(ca_pem: &[u8]) -> Result<Arc<rustls::ClientConfig>> {
    let mut roots = rustls::RootCertStore::empty();
    let native = rustls_native_certs::load_native_certs();
    if !native.errors.is_empty() {
        log::warn!(
            "could not load every OS root certificate: {:?}",
            native.errors
        );
    }
    let (native_added, native_ignored) = roots.add_parsable_certificates(native.certs);
    log::debug!("trusting {native_added} OS root certificate(s); ignored {native_ignored}");
    let mut reader = io::BufReader::new(ca_pem);
    let certificates = rustls_pemfile::certs(&mut reader)
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(|error| {
            LoomError::invalid_request(format!("could not parse the TLS CA PEM: {error}"))
        })?;
    if certificates.is_empty() {
        return Err(LoomError::invalid_request(
            "the TLS CA PEM contains no certificates",
        ));
    }
    let (ca_added, _) = roots.add_parsable_certificates(certificates);
    if ca_added == 0 {
        return Err(LoomError::invalid_request(
            "the TLS CA PEM contains no usable certificates",
        ));
    }
    Ok(Arc::new(
        rustls::ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth(),
    ))
}

/// The host of a plaintext `ws://` endpoint that is not the local machine.
///
/// `wss://` endpoints carry TLS and loopback endpoints never leave the machine,
/// so neither needs the insecure-remote opt-in. An unparseable URL is left to
/// the connection attempt to reject.
fn insecure_remote_host(url: &str) -> Option<String> {
    let uri = url
        .parse::<tokio_tungstenite::tungstenite::http::Uri>()
        .ok()?;
    if !uri
        .scheme_str()
        .is_some_and(|scheme| scheme.eq_ignore_ascii_case("ws"))
    {
        return None;
    }
    let host = uri.host()?;
    let host = host.trim_start_matches('[').trim_end_matches(']');
    if host.eq_ignore_ascii_case("localhost")
        || host
            .parse::<IpAddr>()
            .is_ok_and(|address| address.is_loopback())
    {
        return None;
    }
    Some(host.to_owned())
}

/// Substrings that identify a rustls certificate or handshake failure, which
/// tokio-tungstenite reports as an opaque I/O error.
const TLS_FAILURE_MARKERS: [&str; 7] = [
    "invalid peer certificate",
    "certificate",
    "invalid dns name",
    "no certificates presented",
    "peer sent no certificates",
    "received fatal alert",
    "handshake failure",
];

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
    } else if matches!(
        error,
        TungsteniteError::Url(
            tokio_tungstenite::tungstenite::error::UrlError::TlsFeatureNotEnabled
        )
    ) {
        LoomError::new(
            ErrorCode::Internal,
            "this WebSocket client was built without TLS support and cannot use a wss:// endpoint; \
             connect to a loopback worker over ws:// or install a build with the rustls TLS feature",
            false,
        )
    } else if let Some(detail) = tls_failure_detail(&error) {
        LoomError::new(
            ErrorCode::Internal,
            format!(
                "TLS handshake with the worker failed ({detail}); verify that the worker's \
                 certificate is valid for its hostname and trust its issuing CA with --ca or \
                 LOOM_TLS_CA"
            ),
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

/// The rustls failure behind a connection error, when there is one.
fn tls_failure_detail(error: &TungsteniteError) -> Option<String> {
    match error {
        TungsteniteError::Tls(tls) => Some(tls.to_string()),
        TungsteniteError::Io(io_error) => {
            let message = io_error.to_string();
            let lowercase = message.to_ascii_lowercase();
            TLS_FAILURE_MARKERS
                .iter()
                .any(|marker| lowercase.contains(marker))
                .then_some(message)
        }
        _ => None,
    }
}

#[cfg(test)]
mod unit_tests {
    use super::{
        CLOSE_MESSAGE_TOO_BIG, ConnectionControl, ConnectionLimits, DEFAULT_MAX_FRAME_BYTES,
        DEFAULT_MAX_IN_FLIGHT_CONTROL_REQUESTS, DEFAULT_MAX_IN_FLIGHT_REQUESTS, InFlightLimits,
        MAX_FRAME_BYTES_ENV, MAX_FRAME_BYTES_FLAG, OversizedFrameAction, RemoteServer,
        RemoteServerConfig, RequestClass, ServerTlsConfig, WEBSOCKET_TLS_SUPPORTED,
        WebSocketTransport, abandoned_request_error, auth_error_response, bearer_token,
        client_tls_config, execute_request_with, frame_request_id, frame_too_large_close_reason,
        frame_too_large_error, insecure_remote_host, is_control_or_interrupt_request,
        oversized_inbound_frame_action, process_text, request_auth, resolve_max_frame_bytes,
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
        ResponseEnvelope, RunRequest, ServerResponse, WorkspaceRequest, decode_response,
        encode_client_frame, encode_request, encode_response,
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
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpStream,
        sync::mpsc,
    };
    use tokio_tungstenite::tungstenite::Error as TungsteniteError;

    use super::super::AuthorizationScope;
    use super::{AuthTokenStore, InProcessBackend};
    use tokio_tungstenite::{
        connect_async,
        tungstenite::{Error as TungsteniteClientError, error::UrlError, http::StatusCode},
    };

    /// Test-only TLS fixtures: a CA and a leaf for `127.0.0.1`/`localhost` that
    /// are valid until 2126. They exist only so the end-to-end TLS test can run
    /// without reaching the network or a real certificate authority.
    const TEST_CERT_PEM: &[u8] = include_bytes!("../tests/fixtures/tls/server-cert.pem");
    const TEST_KEY_PEM: &[u8] = include_bytes!("../tests/fixtures/tls/server-key.pem");
    const TEST_CA_PEM: &[u8] = include_bytes!("../tests/fixtures/tls/ca-cert.pem");

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
        let wrapped =
            encode_client_frame(&ClientFrame::Request(Box::new(request.clone()))).unwrap();
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
        let second =
            RequestEnvelope::new(ClientRequest::Control(ControlRequest::DiscoverCapabilities));
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

        let error =
            ServerTlsConfig::from_pem_files(fixture("missing.pem"), fixture("server-key.pem"))
                .unwrap_err();
        assert_eq!(error.code, ErrorCode::InvalidRequest);
        assert!(
            error
                .message
                .contains("could not read the TLS certificate file"),
            "{error}"
        );
        let error =
            ServerTlsConfig::from_pem_files(fixture("server-cert.pem"), fixture("missing.pem"))
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
        let request =
            format!("GET {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n");
        stream.write_all(request.as_bytes()).await.unwrap();
        let mut response = Vec::new();
        stream.read_to_end(&mut response).await.unwrap();
        String::from_utf8_lossy(&response).into_owned()
    }
}
