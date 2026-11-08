use super::*;

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
    /// Who may sign in to this listener with a GitHub account, and where the
    /// device flow and account verification call go.
    ///
    /// Sign-in is enabled unless the policy is switched off, and an empty
    /// allowlist then accepts any GitHub account. A WebSocket upgrade without a
    /// credential is accepted as a pre-login connection, which may only sign in
    /// until a person is bound to it; a disabled policy refuses such an upgrade,
    /// exactly as it was before sign-in existed.
    pub github_sign_in: GitHubSignInConfig,
    /// Most connections may be accepted without a credential, waiting to sign
    /// in, at once. It defaults to [`DEFAULT_MAX_ANONYMOUS_CONNECTIONS`].
    ///
    /// A slot is released as soon as the connection is bound to a person, so a
    /// signed-in client does not hold one.
    pub max_anonymous_connections: usize,
    /// How long such a connection may wait to sign in before the listener
    /// closes it. It defaults to [`DEFAULT_SIGN_IN_TIMEOUT`].
    pub sign_in_timeout: Duration,
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
            github_sign_in: GitHubSignInConfig::default(),
            max_anonymous_connections: DEFAULT_MAX_ANONYMOUS_CONNECTIONS,
            sign_in_timeout: DEFAULT_SIGN_IN_TIMEOUT,
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
    pub(super) backend: Arc<InProcessBackend>,
    pub(super) auth: Arc<AuthTokenStore>,
    pub(super) config: RemoteServerConfig,
}

impl RemoteServer {
    pub fn new(
        backend: Arc<InProcessBackend>,
        auth: Arc<AuthTokenStore>,
        config: RemoteServerConfig,
    ) -> Self {
        // The store that authenticates this listener's connections also issues
        // and revokes the credentials a GitHub sign-in hands out, and restores
        // the ones stored before the last restart.
        backend.attach_person_token_store(Arc::clone(&auth));
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
            || self.config.max_anonymous_connections == 0
        {
            return Err(LoomError::invalid_request(
                "remote server limits must be greater than zero",
            ));
        }
        // The sign-in policy lives on the backend a connection serves, so the
        // listener installs the one it was started with before accepting
        // anything.
        self.backend
            .set_github_sign_in_config(self.config.github_sign_in.clone());
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
            anonymous: Arc::new(Semaphore::new(self.config.max_anonymous_connections)),
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
    pub(super) fn check_exposure(&self) -> Result<()> {
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

pub(super) fn server_stopped_error(error: io::Error) -> LoomError {
    LoomError::new(
        ErrorCode::Internal,
        format!("Loom WebSocket server stopped unexpectedly: {error}"),
        true,
    )
}

pub struct RunningRemoteServer {
    pub(super) local_addr: SocketAddr,
    pub(super) websocket_url: String,
    pub(super) tls_enabled: bool,
    pub(super) shutdown_sender: Option<oneshot::Sender<()>>,
    pub(super) task: JoinHandle<Result<()>>,
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
pub(super) struct RemoteState {
    pub(super) backend: Arc<InProcessBackend>,
    pub(super) auth: Arc<AuthTokenStore>,
    pub(super) config: RemoteServerConfig,
    /// Bounds how many connections may be waiting to sign in at once.
    pub(super) anonymous: Arc<Semaphore>,
}

/// Reports whether the backend can still serve durable requests.
///
/// A backend that latched its persistence fail-stop refuses every write until
/// it is reopened, so an unhealthy listener must not keep answering `ok`: an
/// uptime check or orchestrator uses this to restart it instead.
pub(super) async fn health_handler(State(state): State<RemoteState>) -> impl IntoResponse {
    match state.backend.ensure_persistence_healthy() {
        Ok(()) => (StatusCode::OK, "ok"),
        Err(_) => (StatusCode::SERVICE_UNAVAILABLE, "persistence fail-stop"),
    }
}

/// Subprotocol prefix used by browser clients that cannot set an
/// `Authorization` header during the WebSocket handshake. The token travels in
/// `Sec-WebSocket-Protocol`, never in the URL, so it cannot leak through
/// request logs, referrers, or the address bar.
pub(super) const BEARER_SUBPROTOCOL_PREFIX: &str = "loom.bearer.";

pub(super) async fn websocket_handler(
    mut upgrade: WebSocketUpgrade,
    headers: HeaderMap,
    State(state): State<RemoteState>,
) -> Response {
    let sign_in_enabled = state.backend.github_sign_in_enabled();
    let (auth, subprotocol) = match request_auth(&headers) {
        Ok((token, subprotocol)) => match state.auth.authenticate(token) {
            Ok(auth) => (Some(auth), subprotocol),
            Err(error) => return auth_error_response(error),
        },
        // An upgrade carrying no credential at all is accepted only when this
        // worker lets people sign in with GitHub, and then only as a pre-login
        // connection that can do nothing but sign in.
        Err(error) if sign_in_enabled && error.code == ErrorCode::AuthenticationRequired => {
            (None, None)
        }
        Err(error) => return auth_error_response(error),
    };
    if let Some(protocol) = subprotocol {
        // Echo the selected subprotocol so the browser accepts the upgrade.
        upgrade = upgrade.protocols([protocol.to_owned()]);
    }
    let anonymous = match &auth {
        Some(_) => None,
        None => match Arc::clone(&state.anonymous).try_acquire_owned() {
            Ok(permit) => Some(permit),
            Err(_) => {
                log::warn!(
                    "refused a WebSocket upgrade without a credential: this listener already \
                     has {} connections waiting to sign in",
                    state.config.max_anonymous_connections
                );
                return (
                    StatusCode::SERVICE_UNAVAILABLE,
                    "too many connections are waiting to sign in",
                )
                    .into_response();
            }
        },
    };
    let connection = match auth {
        Some(auth) => state.backend.connect_authenticated(auth),
        None => state.backend.connect_pre_login(),
    };
    upgrade
        .on_upgrade(move |socket| handle_socket(socket, state, connection, anonymous))
        .into_response()
}

/// Resolves the bearer token for an incoming WebSocket upgrade.
///
/// Native clients send an `Authorization: Bearer` header. Browser `WebSocket`
/// clients cannot set arbitrary handshake headers, so they carry the token in a
/// `loom.bearer.<token>` subprotocol. The header takes precedence when both are
/// present.
pub(super) fn request_auth(headers: &HeaderMap) -> Result<(&str, Option<&str>)> {
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

pub(super) fn bearer_token(headers: &HeaderMap) -> Result<&str> {
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

pub(super) fn auth_error_response(error: LoomError) -> Response {
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

pub(super) enum ConnectionControl {
    Backpressure,
}

/// Which in-flight permit pool a request draws on.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum RequestClass {
    /// A request that does work but is not how a client observes or stops it.
    Ordinary,
    /// A request that tells a client what the worker can do or stops work that
    /// is already running; see [`is_control_or_interrupt_request`].
    Control,
}

impl RequestClass {
    pub(super) fn of(request: &ClientRequest) -> Self {
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
pub(super) fn is_control_or_interrupt_request(request: &ClientRequest) -> bool {
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
pub(super) struct InFlightLimits {
    pub(super) ordinary: Arc<Semaphore>,
    pub(super) control: Arc<Semaphore>,
    pub(super) max_ordinary: usize,
    pub(super) max_control: usize,
}

impl InFlightLimits {
    /// The limits come from [`RemoteServerConfig::max_in_flight_requests`] and
    /// [`RemoteServerConfig::max_in_flight_control_requests`], which
    /// [`RemoteServer::bind`] requires to be greater than zero.
    pub(super) fn new(max_ordinary: usize, max_control: usize) -> Self {
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
    pub(super) fn admit(
        &self,
        request_id: RequestId,
        class: RequestClass,
    ) -> Result<OwnedSemaphorePermit> {
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
pub(super) struct ConnectionLimits {
    /// Most bytes an inbound frame may carry; see [`DEFAULT_MAX_FRAME_BYTES`].
    pub(super) max_frame_bytes: usize,
    /// How long a request may wait for its blocking operation.
    pub(super) request_timeout: Duration,
    pub(super) in_flight: InFlightLimits,
}

impl ConnectionLimits {
    pub(super) fn new(config: &RemoteServerConfig) -> Self {
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
pub(super) fn abandoned_request_error(
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

pub(super) async fn handle_socket(
    socket: WebSocket,
    state: RemoteState,
    connection: InProcessConnection,
    // The slot a listener reserves for a connection that has no credential yet.
    // Released once a person is bound to the connection, see the loop below.
    mut anonymous: Option<OwnedSemaphorePermit>,
) {
    // The connection carries the listener's frame limit, so an events batch it
    // answers with is bounded to a frame its clients accept instead of reaching
    // them as an over-limit frame that their own rule drains.
    let connection = connection.with_max_response_bytes(state.config.max_frame_bytes);
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
    // A person losing access ends this connection, and with it every live
    // event subscription it carries. The client reconnects and is then held to
    // its remaining grants. Operator connections are never signalled.
    //
    // Identity is mutable: a GitHub sign-in binds a person to a connection that
    // was accepted without a credential, and a sign-out releases them again. The
    // presence and its revocation signal are therefore re-read on every wait
    // instead of captured once.
    let sign_in_deadline = connection
        .awaits_sign_in()
        .then(|| Instant::now() + state.config.sign_in_timeout);
    let touch = || {
        if let Some(presence) = connection.presence() {
            presence.touch();
        }
    };

    loop {
        // A connection that signed in no longer waits for anything, so it gives
        // its pre-login slot back instead of holding one for the whole session.
        if anonymous.is_some() && connection.participant().is_some() {
            anonymous = None;
        }
        let presence = connection.presence();
        let revoked = presence.as_ref().map(|presence| presence.revoked());
        let revoked_wait = async {
            match &revoked {
                Some(revoked) => revoked.notified().await,
                None => std::future::pending().await,
            }
        };
        tokio::pin!(revoked_wait);

        tokio::select! {
            () = &mut revoked_wait => {
                let _ = message_sender
                    .send(Message::Close(Some(CloseFrame {
                        code: CLOSE_POLICY_VIOLATION,
                        reason: "session access was revoked".into(),
                    })))
                    .await;
                break;
            }
            control_message = control_receiver.recv() => {
                if matches!(control_message, Some(ConnectionControl::Backpressure)) {
                    break;
                }
            }
            _ = heartbeat.tick() => {
                if last_activity.elapsed() > heartbeat_timeout {
                    break;
                }
                // A connection that was accepted without a credential must sign
                // in within its window instead of holding its slot forever.
                if let Some(deadline) = sign_in_deadline
                    && connection.awaits_sign_in()
                    && Instant::now() >= deadline
                {
                    let _ = message_sender
                        .send(Message::Close(Some(CloseFrame {
                            code: CLOSE_POLICY_VIOLATION,
                            reason: "sign-in timed out".into(),
                        })))
                        .await;
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
                        touch();
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
                        touch();
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
                        touch();
                        if message_sender.try_send(Message::Pong(payload)).is_err() {
                            break;
                        }
                    }
                    Ok(Message::Pong(_)) => {
                        last_activity = Instant::now();
                        touch();
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
    // Dropping the connection's last clone removes it from the roster; the
    // aborted tasks above released theirs.
    drop(connection);
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
pub(super) async fn process_text(
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
pub(super) fn frame_request_id(bytes: &[u8]) -> Option<RequestId> {
    if let Ok(request) = decode_request(bytes) {
        return Some(request.request_id);
    }
    match decode_client_frame(bytes) {
        Ok(ClientFrame::Request(request)) => Some(request.request_id),
        Ok(ClientFrame::Cancel { request_id }) => Some(request_id),
        Err(_) => None,
    }
}

pub(super) async fn execute_request(
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
pub(super) async fn execute_request_with(
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

pub(super) fn try_send_response(
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
