//! Transport-independent protocol client for the native shell and browser.
//!
//! Native builds talk to either an in-process backend or a remote backend
//! over a native WebSocket (via `loom-server`'s blocking client). The browser
//! build only ever talks to a remote backend, over a real `web_sys::WebSocket`
//! (see `browser::BrowserConnection`), since it cannot spawn an in-process
//! backend or open a native socket. Both variants share one request/response
//! shape so the UI (`view.rs`) never needs to know which transport it's
//! using.

use std::collections::BTreeMap;
#[cfg(not(target_family = "wasm"))]
use std::time::Duration;

#[cfg(target_family = "wasm")]
use futures_channel::oneshot;
use loom_core::{
    AgentSessionSnapshot, Capability, CapabilitySet, ErrorCode, LoomError, ParticipantId,
    ParticipantIdentity, WorkspaceId, WorkspaceRecord,
};
use loom_model::{ModelId, ProviderId, ProviderSummary};
#[cfg(not(target_family = "wasm"))]
use loom_protocol::AgentRunSnapshot;
use loom_protocol::{
    AccessRequest, AccessResponse, CURRENT_PROTOCOL_VERSION, ClientRequest, ControlRequest,
    ControlResponse, NegotiationResult, ProviderRequest, ProviderResponse, RepositoryRequest,
    RepositoryResponse, RequestEnvelope, ResponseEnvelope, ServerResponse, SessionRepository,
    SessionResponse, WorkerNodeStatus, WorkspaceConfig, WorkspaceRequest, WorkspaceResponse,
};
#[cfg(not(target_family = "wasm"))]
use loom_protocol::{RunRequest, RunResponse};
#[cfg(target_family = "wasm")]
use std::task::Poll;

pub(crate) use crate::backend_host::BackendWorker;

pub(crate) fn redact_secret(value: &str, secret: &str) -> String {
    if secret.is_empty() {
        return value.to_owned();
    }
    let mut encoded = String::new();
    for byte in secret.bytes() {
        if byte.is_ascii_alphanumeric() || b"-_.!~*'()".contains(&byte) {
            encoded.push(char::from(byte));
        } else {
            use std::fmt::Write;
            let _ = write!(encoded, "%{byte:02X}");
        }
    }
    value
        .replace(secret, "[redacted]")
        .replace(&encoded, "[redacted]")
        .replace(&encoded.to_ascii_lowercase(), "[redacted]")
}

pub(crate) fn remote_url_is_secure_for_secrets(url: &str) -> bool {
    let (scheme, rest) = if let Some(rest) = url.strip_prefix("wss://") {
        ("wss", rest)
    } else if let Some(rest) = url.strip_prefix("ws://") {
        ("ws", rest)
    } else {
        return false;
    };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    if authority.is_empty() || authority.contains('@') {
        return false;
    }
    if scheme == "wss" {
        // Answer for the transport rather than for the URL: the native client
        // negotiates TLS with rustls over the OS roots plus any configured CA,
        // and the browser hands it to the platform WebSocket, so a `wss://`
        // endpoint really is encrypted.
        return native_transport_tls_supported();
    }
    let host = if let Some(bracketed_host) = authority.strip_prefix('[') {
        bracketed_host.split(']').next().unwrap_or_default()
    } else {
        authority
            .rsplit_once(':')
            .map_or(authority, |(host, _)| host)
    };
    host == "localhost"
        || host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|address| address.is_loopback())
}

/// Whether this build's WebSocket transport can negotiate TLS itself.
///
/// Reported instead of trusting the URL scheme so the answer tracks what the
/// transport can do. Native builds re-export the backend transport's capability;
/// the browser only ever uses the platform `WebSocket`, which always supports
/// `wss://`.
#[cfg(not(target_family = "wasm"))]
fn native_transport_tls_supported() -> bool {
    loom_local::WEBSOCKET_TLS_SUPPORTED
}

#[cfg(target_family = "wasm")]
fn native_transport_tls_supported() -> bool {
    true
}

/// Prefix for the bearer token subprotocol. A browser `WebSocket` cannot set an
/// `Authorization` header during the handshake, so the token travels in
/// `Sec-WebSocket-Protocol` instead of the URL.
///
/// Native builds carry it only so the handshake rule is tested once for both
/// transports.
#[cfg_attr(not(target_family = "wasm"), allow(dead_code))]
pub(crate) const BEARER_SUBPROTOCOL_PREFIX: &str = "loom.bearer.";

/// The subprotocols a browser handshake offers for `token`.
///
/// An empty token offers none at all: the upgrade then carries no credential
/// and the worker accepts it as a pre-login connection that may sign in with
/// GitHub. Offering an empty `loom.bearer.` protocol instead would read as a
/// failed credential.
#[cfg_attr(not(target_family = "wasm"), allow(dead_code))]
pub(crate) fn browser_subprotocols(token: &str) -> Vec<String> {
    if token.is_empty() {
        Vec::new()
    } else {
        vec![format!("{BEARER_SUBPROTOCOL_PREFIX}{token}")]
    }
}

#[cfg(not(target_family = "wasm"))]
pub(crate) fn describe_startup_connection_error(error: &LoomError, secret: &str) -> String {
    match error.code {
        ErrorCode::AuthenticationFailed
        | ErrorCode::AuthenticationRequired
        | ErrorCode::AuthorizationDenied => {
            "worker authentication was denied; verify the access token and worker authentication configuration".to_owned()
        }
        ErrorCode::InvalidRequest
            if error
                .message
                .to_ascii_lowercase()
                .contains("bearer token") =>
        {
            "worker access token contains unsupported characters; verify the token value".to_owned()
        }
        ErrorCode::InvalidRequest => {
            "invalid worker WebSocket URL; use a ws:// or wss:// URL with the correct endpoint path".to_owned()
        }
        ErrorCode::DeadlineExceeded => {
            "worker connection timed out; check that it is reachable and retry".to_owned()
        }
        ErrorCode::ProviderUnavailable
            if error.message.to_ascii_lowercase().contains("refused") =>
        {
            "connection refused; check that the worker is running and the URL and port are correct".to_owned()
        }
        ErrorCode::UnsupportedProtocol | ErrorCode::UnsupportedCapability => {
            "worker protocol negotiation failed; update the worker and UI to compatible versions".to_owned()
        }
        ErrorCode::RequestCancelled => {
            "worker closed the connection during startup; check the URL, worker availability, and access token".to_owned()
        }
        _ => redact_secret(&error.to_string(), secret),
    }
}

#[cfg(target_family = "wasm")]
use crate::browser::BrowserConnection;
#[cfg(target_family = "wasm")]
use wasm_bindgen::{JsCast, closure::Closure};

#[derive(Clone)]
pub(crate) enum ClientConnection {
    /// An in-process backend; only tests use it, through the `loom-server`
    /// dev-dependency. The native client always reaches its backend over
    /// WebSocket, including the managed local worker.
    #[cfg(all(not(target_family = "wasm"), test))]
    InProcess(Box<loom_server::InProcessConnection>),
    #[cfg(not(target_family = "wasm"))]
    Remote(loom_local::RemoteConnection),
    #[cfg(target_family = "wasm")]
    Disconnected,
    #[cfg(target_family = "wasm")]
    Browser(BrowserConnection),
}

pub(crate) struct ConnectionCleanupGuard {
    connection: ClientConnection,
    armed: bool,
}

impl ConnectionCleanupGuard {
    pub(crate) fn new(connection: ClientConnection) -> Self {
        Self {
            connection,
            armed: true,
        }
    }

    pub(crate) fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for ConnectionCleanupGuard {
    fn drop(&mut self) {
        if self.armed && self.connection.close().is_err() {
            log::warn!("failed to close a partially initialized worker transport");
        }
    }
}

impl ClientConnection {
    #[cfg(not(target_family = "wasm"))]
    pub(crate) fn remote(
        url: String,
        token: String,
        options: &loom_local::RemoteConnectionOptions,
    ) -> Result<Self, LoomError> {
        let secure_for_secrets = remote_url_is_secure_for_secrets(&url);
        Ok(Self::Remote(loom_local::RemoteConnection::connect(
            &url,
            &token,
            secure_for_secrets,
            options,
        )?))
    }

    #[cfg(target_family = "wasm")]
    pub(crate) fn browser(url: &str, token: &str) -> Result<Self, LoomError> {
        Ok(Self::Browser(BrowserConnection::connect(url, token)?))
    }

    pub(crate) fn close(&self) -> Result<(), LoomError> {
        #[cfg(not(target_family = "wasm"))]
        {
            match self {
                #[cfg(test)]
                Self::InProcess(_) => Ok(()),
                Self::Remote(connection) => connection.close(),
            }
        }
        #[cfg(target_family = "wasm")]
        {
            match self {
                Self::Disconnected => Ok(()),
                Self::Browser(connection) => connection.close(),
            }
        }
    }

    pub(crate) fn secure_for_secrets(&self) -> bool {
        match self {
            #[cfg(all(not(target_family = "wasm"), test))]
            Self::InProcess(_) => true,
            #[cfg(not(target_family = "wasm"))]
            Self::Remote(connection) => connection.secure_for_secrets(),
            #[cfg(target_family = "wasm")]
            Self::Disconnected => false,
            #[cfg(target_family = "wasm")]
            Self::Browser(connection) => connection.secure_for_secrets(),
        }
    }

    /// The reason the transport is known to be closed, if any.
    ///
    /// The browser transport reports the close so the view can replace a
    /// silently-dead workspace with a reconnect screen instead of failing
    /// every later request. Native transports manage their own lifecycle.
    #[cfg(target_family = "wasm")]
    pub(crate) fn closed_reason(&self) -> Option<String> {
        match self {
            Self::Disconnected => Some("no worker is connected".to_owned()),
            Self::Browser(connection) => connection.closed_reason(),
        }
    }

    /// Sends a request and blocks the calling thread for its response.
    ///
    /// Only used natively, where callers either run this on a dedicated
    /// worker thread (`BackendWorker`) or, during the synchronous startup
    /// bootstrap, before any window exists. A remote request is bounded by
    /// [`loom_local::DEFAULT_REQUEST_TIMEOUT`] so a half-open socket cannot
    /// stall the worker thread — and with it every later request — forever;
    /// in-process requests are unbounded because they do not depend on a
    /// socket staying alive.
    #[cfg(not(target_family = "wasm"))]
    pub(crate) fn request(&self, request: RequestEnvelope) -> ResponseEnvelope {
        self.request_with_timeout(request, loom_local::DEFAULT_REQUEST_TIMEOUT)
    }

    #[cfg(not(target_family = "wasm"))]
    pub(crate) fn request_with_timeout(
        &self,
        request: RequestEnvelope,
        timeout: Duration,
    ) -> ResponseEnvelope {
        match self {
            #[cfg(test)]
            Self::InProcess(connection) => connection.request(request),
            Self::Remote(connection) => connection.request_with_timeout(request, timeout),
        }
    }

    /// Sends a request and awaits its response. Only used in the browser,
    /// where nothing can block the page's single JS thread; the WebSocket
    /// transport resolves this asynchronously as frames arrive.
    #[cfg(target_family = "wasm")]
    pub(crate) async fn request(&self, request: RequestEnvelope) -> ResponseEnvelope {
        let request_id = request.request_id;
        match self {
            Self::Disconnected => ResponseEnvelope::failure(
                request_id,
                LoomError::new(ErrorCode::RequestCancelled, "no worker is connected", true),
            ),
            Self::Browser(connection) => connection.request(request).await,
        }
    }
}

/// The capabilities every client transport negotiates.
///
/// One list for both transports so a capability added for the native client is
/// never silently missing from the browser client (or the reverse). The
/// transport-specific entries live in `NATIVE_ONLY_NEGOTIATION_CAPABILITIES`,
/// each with the reason it is native-only.
const SHARED_NEGOTIATION_CAPABILITIES: &[Capability] = &[
    Capability::CreateAgentSession,
    Capability::ReadAgentSession,
    Capability::ControlAgentSession,
    Capability::DeleteAgentSession,
    Capability::ConfigureArchiveRetention,
    Capability::ManageWorkspaces,
    Capability::ManageSessionRepositories,
    Capability::ReadSessionFilesystem,
    Capability::WriteSessionFilesystem,
    Capability::SubscribeSessionEvents,
    Capability::SubscribeWorkspaceEvents,
    Capability::StartAgentRun,
    Capability::ReadAgentRun,
    Capability::ReadAgentRunMessages,
    Capability::ControlAgentRun,
    Capability::PauseAgentRun,
    Capability::ResumeAgentRun,
    Capability::ApproveAgentAction,
    Capability::ListProviders,
    Capability::ConfigureProviders,
    Capability::ReadWorkspaceConfig,
    Capability::BrowseGitHubRepositories,
    // The roster, composer, and sign-in-dialog prompts key off the negotiated
    // capability, so a client that omits it is told the worker offers no
    // sign-in even when it does. A worker that disables sign-in still reports
    // the refusal when a sign-in starts.
    Capability::GitHubSignIn,
    Capability::ConfigureApprovalPolicy,
    Capability::ReadVcsStatus,
    Capability::ReadVcsDiff,
    Capability::ReadUsage,
    Capability::InspectContext,
    Capability::ReadSessionTask,
    Capability::StartSessionTask,
    Capability::ControlSessionTask,
    Capability::ReadSessionTaskEvidence,
    Capability::ReadWorkerNodeStatus,
    Capability::ReadProject,
    Capability::SendProjectBranchMessage,
    Capability::ReadProjectAgentMessages,
    Capability::ControlProjectChild,
    Capability::CreateProjectWorktree,
    Capability::ReadProjectChildReview,
    Capability::IntegrateProjectChild,
    Capability::CleanupProjectChildWorktree,
    // Access and collaboration writes are capability-checked before dispatch
    // (`authorize_request_access` in the worker), so a client that omits them
    // cannot grant roles, request or approve access, or send peer messages:
    // those retryable mutations fail with `CapabilityDenied` instead.
    Capability::ReadSessionRoster,
    Capability::ManageSessionAccess,
    Capability::RequestSessionAccess,
    Capability::SendPeerMessage,
    Capability::JsonProtocol,
];

/// Capabilities only the native transports negotiate, each with the reason the
/// browser transport does not.
///
/// A documented difference is allowed; an undocumented one is not, which is
/// what the `capability_lists_agree_except_for_documented_transport_differences`
/// test checks.
#[cfg(not(target_family = "wasm"))]
const NATIVE_ONLY_NEGOTIATION_CAPABILITIES: &[(Capability, &str)] = &[(
    Capability::ManageCheckpoints,
    "checkpoint management is implemented by the native transports; the browser client has no \
     checkpoint surface",
)];

/// The capabilities this build advertises to a worker during negotiation.
pub(crate) fn negotiation_capabilities() -> CapabilitySet {
    // Chained rather than extended so the list needs no `mut` on the target
    // whose transport-specific entries are compiled out.
    #[cfg(not(target_family = "wasm"))]
    let transport_specific = NATIVE_ONLY_NEGOTIATION_CAPABILITIES
        .iter()
        .map(|(capability, _)| *capability);
    #[cfg(target_family = "wasm")]
    let transport_specific = std::iter::empty::<Capability>();

    CapabilitySet::new(
        SHARED_NEGOTIATION_CAPABILITIES
            .iter()
            .copied()
            .chain(transport_specific),
    )
}

/// The identity the worker bound to this connection.
///
/// `None` means no person is bound: an operator token keeps its unrestricted
/// authority without a person, and a connection accepted without a credential
/// is still pre-login. The caller tells those apart by whether it sent a
/// token, and treats only the second as waiting for a GitHub sign-in.
#[cfg(not(target_family = "wasm"))]
pub(crate) fn current_participant(
    connection: &ClientConnection,
) -> Result<Option<ParticipantIdentity>, LoomError> {
    let response = connection.request_with_timeout(
        RequestEnvelope::new(ClientRequest::Access(AccessRequest::GetCurrentParticipant)),
        Duration::from_secs(15),
    );
    match response.result? {
        ServerResponse::Access(AccessResponse::CurrentParticipant { participant }) => {
            Ok(participant)
        }
        response => Err(unexpected_response("current participant", response)),
    }
}

#[cfg(target_family = "wasm")]
pub(crate) async fn current_participant_async(
    connection: &ClientConnection,
) -> Result<Option<ParticipantIdentity>, LoomError> {
    let response = request_with_timeout(
        connection,
        RequestEnvelope::new(ClientRequest::Access(AccessRequest::GetCurrentParticipant)),
        BROWSER_REQUEST_TIMEOUT_MILLIS,
    )
    .await?;
    match response.result? {
        ServerResponse::Access(AccessResponse::CurrentParticipant { participant }) => {
            Ok(participant)
        }
        response => Err(unexpected_response("current participant", response)),
    }
}

/// Asks the worker to bind this connection back to a remembered participant.
///
/// `Ok(None)` means the worker no longer knows that person, so the caller can
/// forget the saved id. Other errors are left to the caller: a connection that
/// is already bound or a transient failure should keep the saved identity for
/// the next reconnect.
#[cfg(not(target_family = "wasm"))]
pub(crate) fn resume_github_identity(
    connection: &ClientConnection,
    participant_id: ParticipantId,
) -> Result<Option<ParticipantIdentity>, LoomError> {
    let response = connection.request_with_timeout(
        RequestEnvelope::new(ClientRequest::Access(AccessRequest::ResumeGitHubIdentity {
            participant_id,
        })),
        Duration::from_secs(15),
    );
    match response.result {
        Ok(ServerResponse::Access(AccessResponse::GitHubIdentityResumed { participant })) => {
            Ok(Some(participant))
        }
        Err(error) if error.code == ErrorCode::NotFound => Ok(None),
        Err(error) => Err(error),
        Ok(response) => Err(unexpected_response("resume GitHub identity", response)),
    }
}

#[cfg(target_family = "wasm")]
pub(crate) async fn resume_github_identity_async(
    connection: &ClientConnection,
    participant_id: ParticipantId,
) -> Result<Option<ParticipantIdentity>, LoomError> {
    let response = request_with_timeout(
        connection,
        RequestEnvelope::new(ClientRequest::Access(AccessRequest::ResumeGitHubIdentity {
            participant_id,
        })),
        BROWSER_REQUEST_TIMEOUT_MILLIS,
    )
    .await?;
    match response.result {
        Ok(ServerResponse::Access(AccessResponse::GitHubIdentityResumed { participant })) => {
            Ok(Some(participant))
        }
        Err(error) if error.code == ErrorCode::NotFound => Ok(None),
        Err(error) => Err(error),
        Ok(response) => Err(unexpected_response("resume GitHub identity", response)),
    }
}

/// Opens the native transport for `url` with the credential this connection may
/// present.
///
/// A connection with no operator token replays the GitHub sign-in the client
/// kept for that worker, so a reload signs in as that person instead of asking
/// for a device flow again; every other connection uses its token as before.
///
/// The worker refuses a credential it no longer honours by failing the
/// handshake, which is synchronous natively. A stale credential is therefore
/// cleared and the connection is opened once more without it, which leaves the
/// node pre-login so it shows the ordinary sign-in prompt.
#[cfg(not(target_family = "wasm"))]
pub(crate) fn connect_remote(
    url: &str,
    operator_token: &str,
    options: &loom_local::RemoteConnectionOptions,
    identity_key: &str,
) -> Result<ClientConnection, LoomError> {
    let stored = crate::credential_store::load(identity_key);
    let Some(credential) = crate::credential_store::replayable(operator_token, stored, url) else {
        return ClientConnection::remote(url.to_owned(), operator_token.to_owned(), options);
    };
    match ClientConnection::remote(url.to_owned(), credential, options) {
        Ok(connection) => Ok(connection),
        Err(error) if refused_a_credential(&error) => {
            log::debug!(
                "[loom-ui] the worker refused the kept GitHub sign-in for worker '{}'; \
                 reconnecting without it: {}",
                identity_key,
                error.message
            );
            crate::credential_store::clear(identity_key);
            ClientConnection::remote(url.to_owned(), String::new(), options)
        }
        Err(error) => Err(error),
    }
}

/// Whether `error` is the worker refusing the credential a connection presented.
#[cfg(not(target_family = "wasm"))]
fn refused_a_credential(error: &LoomError) -> bool {
    matches!(
        error.code,
        ErrorCode::AuthenticationFailed
            | ErrorCode::AuthenticationRequired
            | ErrorCode::AuthorizationDenied
    )
}

/// Why a browser connect failed.
///
/// The view reports a transport that could not be created differently from a
/// worker that closed it before negotiation, so the two stay apart here.
#[cfg(target_family = "wasm")]
pub(crate) enum BrowserConnectFailure {
    Transport(LoomError),
    Negotiation(LoomError),
}

#[cfg(target_family = "wasm")]
impl BrowserConnectFailure {
    pub(crate) fn into_error(self) -> LoomError {
        match self {
            Self::Transport(error) | Self::Negotiation(error) => error,
        }
    }
}

/// Opens the browser transport for a bootstrap or a reconnect.
///
/// The browser cannot inspect a handshake refusal: a worker that does not accept
/// the credential closes the socket and the first request reports it. A connect
/// that presented a kept sign-in therefore clears it and retries once without
/// it, so a sign-in revoked elsewhere lands on the ordinary sign-in prompt
/// instead of a dead connection. The retry never repeats.
#[cfg(target_family = "wasm")]
pub(crate) async fn connect_browser(
    url: &str,
    operator_token: &str,
    identity_key: &str,
) -> Result<(ClientConnection, NegotiationResult), BrowserConnectFailure> {
    let stored = crate::credential_store::load(identity_key);
    let kept = crate::credential_store::replayable(operator_token, stored, url);
    let connection = ClientConnection::browser(url, kept.as_deref().unwrap_or(operator_token))
        .map_err(BrowserConnectFailure::Transport)?;
    let error = match negotiate_async(&connection).await {
        Ok(negotiation) => return Ok((connection, negotiation)),
        Err(error) if kept.is_some() => error,
        Err(error) => return Err(BrowserConnectFailure::Negotiation(error)),
    };
    log::debug!(
        "[loom-ui] the worker did not accept the kept GitHub sign-in for worker '{}'; retrying \
         without it: {}",
        identity_key,
        error.message
    );
    crate::credential_store::clear(identity_key);
    if let Err(close) = connection.close() {
        log::debug!("[loom-ui] could not close the rejected worker transport: {close}");
    }
    let retry =
        ClientConnection::browser(url, operator_token).map_err(BrowserConnectFailure::Transport)?;
    let negotiation = negotiate_async(&retry)
        .await
        .map_err(BrowserConnectFailure::Negotiation)?;
    Ok((retry, negotiation))
}

#[cfg(not(target_family = "wasm"))]
pub(crate) fn negotiate(connection: &ClientConnection) -> Result<NegotiationResult, LoomError> {
    let response = connection.request_with_timeout(
        RequestEnvelope::new(ClientRequest::Control(ControlRequest::Negotiate {
            client_version: CURRENT_PROTOCOL_VERSION,
            capabilities: negotiation_capabilities(),
        })),
        Duration::from_secs(15),
    );
    match response.result? {
        ServerResponse::Control(ControlResponse::Negotiated(result)) => Ok(result),
        response => Err(unexpected_response("negotiation", response)),
    }
}

#[cfg(not(target_family = "wasm"))]
pub(crate) fn worker_node_status(
    connection: &ClientConnection,
) -> Result<WorkerNodeStatus, LoomError> {
    let response = connection.request_with_timeout(
        RequestEnvelope::new(ClientRequest::Control(ControlRequest::GetWorkerNodeStatus)),
        Duration::from_secs(15),
    );
    match response.result? {
        ServerResponse::Control(ControlResponse::WorkerNodeStatus(status)) => Ok(status),
        response => Err(unexpected_response("worker node status", response)),
    }
}

#[cfg(target_family = "wasm")]
pub(crate) async fn worker_node_status_async(
    connection: &ClientConnection,
) -> Result<WorkerNodeStatus, LoomError> {
    let response = request_with_timeout(
        connection,
        RequestEnvelope::new(ClientRequest::Control(ControlRequest::GetWorkerNodeStatus)),
        BROWSER_REQUEST_TIMEOUT_MILLIS,
    )
    .await?;
    match response.result? {
        ServerResponse::Control(ControlResponse::WorkerNodeStatus(status)) => Ok(status),
        response => Err(unexpected_response("worker node status", response)),
    }
}

/// Confirms that a browser transport which still looks open really answers.
///
/// A page that was hidden can leave a socket that never reports its close, so
/// the view asks for the worker node status over the same connection and only
/// treats the connection as usable when a well-formed status arrives in time.
#[cfg(target_family = "wasm")]
pub(crate) async fn probe_worker_connection(
    connection: &ClientConnection,
) -> Result<(), LoomError> {
    let response = request_with_timeout(
        connection,
        RequestEnvelope::new(ClientRequest::Control(ControlRequest::GetWorkerNodeStatus)),
        BROWSER_PROBE_TIMEOUT_MILLIS,
    )
    .await?;
    match response.result? {
        ServerResponse::Control(ControlResponse::WorkerNodeStatus(_)) => Ok(()),
        response => Err(unexpected_response("worker node status", response)),
    }
}

/// How long a browser request waits for its response before the caller treats
/// the worker as unreachable.
#[cfg(target_family = "wasm")]
const BROWSER_REQUEST_TIMEOUT_MILLIS: i32 = 15_000;

/// How long the resume probe waits for the worker to confirm a socket that
/// still looks open, kept short so returning to a tab answers quickly.
#[cfg(target_family = "wasm")]
const BROWSER_PROBE_TIMEOUT_MILLIS: i32 = 5_000;

#[cfg(target_family = "wasm")]
async fn request_with_timeout(
    connection: &ClientConnection,
    request: RequestEnvelope,
    timeout_millis: i32,
) -> Result<ResponseEnvelope, LoomError> {
    let request = Box::pin(connection.request(request));
    let (timeout_sender, timeout_receiver) = oneshot::channel::<()>();
    let timeout_callback = Closure::once(move || {
        let _ = timeout_sender.send(());
    });
    let window = web_sys::window().ok_or_else(|| {
        LoomError::new(
            ErrorCode::Internal,
            "could not schedule a browser worker-request timeout",
            false,
        )
    })?;
    let timeout_handle = window
        .set_timeout_with_callback_and_timeout_and_arguments_0(
            timeout_callback.as_ref().unchecked_ref(),
            timeout_millis,
        )
        .map_err(|_| {
            LoomError::new(
                ErrorCode::Internal,
                "could not schedule a browser worker-request timeout",
                false,
            )
        })?;
    let mut request = request;
    let mut timeout_receiver = Box::pin(timeout_receiver);
    let result = std::future::poll_fn(|cx| {
        if let Poll::Ready(response) = request.as_mut().poll(cx) {
            return Poll::Ready(Some(response));
        }
        if timeout_receiver.as_mut().poll(cx).is_ready() {
            return Poll::Ready(None);
        }
        Poll::Pending
    })
    .await;
    // Cancel the pending timer so its callback — and the closure that backs it
    // — is released. The timeout fires only for slow requests, so previously
    // every answered request leaked one JS function object.
    let _ = window.clear_timeout_with_handle(timeout_handle);
    result.ok_or_else(|| {
        LoomError::new(
            ErrorCode::DeadlineExceeded,
            "worker request timed out",
            true,
        )
    })
}

#[cfg(not(target_family = "wasm"))]
pub(crate) fn workspace_config(
    connection: &ClientConnection,
    workspace_id: WorkspaceId,
) -> Result<WorkspaceConfig, LoomError> {
    let response = connection.request(RequestEnvelope::new(ClientRequest::Workspace(
        WorkspaceRequest::GetWorkspaceConfigForWorkspace { workspace_id },
    )));
    match response.result? {
        ServerResponse::Workspace(WorkspaceResponse::WorkspaceConfig(config)) => Ok(config),
        response => Err(unexpected_response("workspace config", response)),
    }
}

#[cfg(target_family = "wasm")]
pub(crate) async fn workspace_config_async(
    connection: &ClientConnection,
    workspace_id: WorkspaceId,
) -> Result<WorkspaceConfig, LoomError> {
    let response = connection
        .request(RequestEnvelope::new(ClientRequest::Workspace(
            WorkspaceRequest::GetWorkspaceConfigForWorkspace { workspace_id },
        )))
        .await;
    match response.result? {
        ServerResponse::Workspace(WorkspaceResponse::WorkspaceConfig(config)) => Ok(config),
        response => Err(unexpected_response("workspace config", response)),
    }
}

#[cfg(not(target_family = "wasm"))]
pub(crate) fn set_workspace_config(
    connection: &ClientConnection,
    workspace_id: WorkspaceId,
    config: WorkspaceConfig,
) -> Result<(), LoomError> {
    let response = connection.request(RequestEnvelope::new(ClientRequest::Workspace(
        WorkspaceRequest::SetWorkspaceConfigForWorkspace {
            workspace_id,
            config,
        },
    )));
    match response.result? {
        ServerResponse::Workspace(WorkspaceResponse::WorkspaceConfigUpdated) => Ok(()),
        response => Err(unexpected_response("workspace config update", response)),
    }
}

#[cfg(target_family = "wasm")]
pub(crate) async fn set_workspace_config_async(
    connection: &ClientConnection,
    workspace_id: WorkspaceId,
    config: WorkspaceConfig,
) -> Result<(), LoomError> {
    let response = connection
        .request(RequestEnvelope::new(ClientRequest::Workspace(
            WorkspaceRequest::SetWorkspaceConfigForWorkspace {
                workspace_id,
                config,
            },
        )))
        .await;
    match response.result? {
        ServerResponse::Workspace(WorkspaceResponse::WorkspaceConfigUpdated) => Ok(()),
        response => Err(unexpected_response("workspace config update", response)),
    }
}

#[cfg(target_family = "wasm")]
pub(crate) async fn negotiate_async(
    connection: &ClientConnection,
) -> Result<NegotiationResult, LoomError> {
    let response = request_with_timeout(
        connection,
        RequestEnvelope::new(ClientRequest::Control(ControlRequest::Negotiate {
            client_version: CURRENT_PROTOCOL_VERSION,
            capabilities: negotiation_capabilities(),
        })),
        BROWSER_REQUEST_TIMEOUT_MILLIS,
    )
    .await?;
    match response.result? {
        ServerResponse::Control(ControlResponse::Negotiated(result)) => Ok(result),
        response => Err(unexpected_response("negotiation", response)),
    }
}

#[cfg(not(target_family = "wasm"))]
pub(crate) fn list_workspaces(
    connection: &ClientConnection,
) -> Result<Vec<WorkspaceRecord>, LoomError> {
    let response = connection.request(RequestEnvelope::new(ClientRequest::Workspace(
        WorkspaceRequest::ListWorkspaces,
    )));
    match response.result? {
        ServerResponse::Workspace(WorkspaceResponse::Workspaces { workspaces }) => Ok(workspaces),
        response => Err(unexpected_response("workspace list", response)),
    }
}

#[cfg(target_family = "wasm")]
pub(crate) async fn list_workspaces_async(
    connection: &ClientConnection,
) -> Result<Vec<WorkspaceRecord>, LoomError> {
    let response = connection
        .request(RequestEnvelope::new(ClientRequest::Workspace(
            WorkspaceRequest::ListWorkspaces,
        )))
        .await;
    match response.result? {
        ServerResponse::Workspace(WorkspaceResponse::Workspaces { workspaces }) => Ok(workspaces),
        response => Err(unexpected_response("workspace list", response)),
    }
}

#[cfg(not(target_family = "wasm"))]
pub(crate) fn create_workspace(
    connection: &ClientConnection,
    name: &str,
) -> Result<WorkspaceRecord, LoomError> {
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

#[cfg(target_family = "wasm")]
pub(crate) async fn create_workspace_async(
    connection: &ClientConnection,
    name: &str,
) -> Result<WorkspaceRecord, LoomError> {
    let response = connection
        .request(RequestEnvelope::new(ClientRequest::Workspace(
            WorkspaceRequest::CreateWorkspace {
                name: name.to_owned(),
            },
        )))
        .await;
    match response.result? {
        ServerResponse::Workspace(WorkspaceResponse::WorkspaceCreated(workspace)) => Ok(workspace),
        response => Err(unexpected_response("workspace creation", response)),
    }
}

#[cfg(not(target_family = "wasm"))]
pub(crate) fn list_workspace_sessions(
    connection: &ClientConnection,
    workspace_id: WorkspaceId,
) -> Result<Vec<AgentSessionSnapshot>, LoomError> {
    let response = connection.request(RequestEnvelope::new(ClientRequest::Workspace(
        WorkspaceRequest::ListWorkspaceSessions {
            workspace_id,
            include_archived: false,
        },
    )));
    match response.result? {
        ServerResponse::Session(SessionResponse::AgentSessions { sessions }) => Ok(sessions),
        response => Err(unexpected_response("workspace session list", response)),
    }
}

#[cfg(target_family = "wasm")]
pub(crate) async fn list_workspace_sessions_async(
    connection: &ClientConnection,
    workspace_id: WorkspaceId,
) -> Result<Vec<AgentSessionSnapshot>, LoomError> {
    let response = connection
        .request(RequestEnvelope::new(ClientRequest::Workspace(
            WorkspaceRequest::ListWorkspaceSessions {
                workspace_id,
                include_archived: false,
            },
        )))
        .await;
    match response.result? {
        ServerResponse::Session(SessionResponse::AgentSessions { sessions }) => Ok(sessions),
        response => Err(unexpected_response("workspace session list", response)),
    }
}

#[cfg(not(target_family = "wasm"))]
pub(crate) fn create_session_in_workspace(
    connection: &ClientConnection,
    workspace_id: WorkspaceId,
    name: &str,
) -> Result<AgentSessionSnapshot, LoomError> {
    let response = connection.request(RequestEnvelope::new(ClientRequest::Workspace(
        WorkspaceRequest::CreateAgentSessionInWorkspace {
            workspace_id,
            name: name.to_owned(),
        },
    )));
    match response.result? {
        ServerResponse::Session(SessionResponse::AgentSessionCreated(snapshot)) => Ok(snapshot),
        response => Err(unexpected_response("workspace session creation", response)),
    }
}

#[cfg(target_family = "wasm")]
pub(crate) async fn create_session_in_workspace_async(
    connection: &ClientConnection,
    workspace_id: WorkspaceId,
    name: &str,
) -> Result<AgentSessionSnapshot, LoomError> {
    let response = connection
        .request(RequestEnvelope::new(ClientRequest::Workspace(
            WorkspaceRequest::CreateAgentSessionInWorkspace {
                workspace_id,
                name: name.to_owned(),
            },
        )))
        .await;
    match response.result? {
        ServerResponse::Session(SessionResponse::AgentSessionCreated(snapshot)) => Ok(snapshot),
        response => Err(unexpected_response("workspace session creation", response)),
    }
}

#[cfg(not(target_family = "wasm"))]
pub(crate) fn attach_session_repository(
    connection: &ClientConnection,
    session_id: loom_core::AgentSessionId,
    source: &str,
    path: &str,
) -> Result<SessionRepository, LoomError> {
    let response = connection.request(RequestEnvelope::new(ClientRequest::Repository(
        RepositoryRequest::AttachSessionRepository {
            session_id,
            source: source.to_owned(),
            path: path.to_owned(),
            revision: None,
            reuse_local: false,
        },
    )));
    match response.result? {
        ServerResponse::Repository(RepositoryResponse::SessionRepositoryAttached(repository)) => {
            Ok(repository)
        }
        response => Err(unexpected_response("repository attachment", response)),
    }
}

#[cfg(target_family = "wasm")]
pub(crate) async fn attach_session_repository_async(
    connection: &ClientConnection,
    session_id: loom_core::AgentSessionId,
    source: &str,
    path: &str,
) -> Result<SessionRepository, LoomError> {
    let response = connection
        .request(RequestEnvelope::new(ClientRequest::Repository(
            RepositoryRequest::AttachSessionRepository {
                session_id,
                source: source.to_owned(),
                path: path.to_owned(),
                revision: None,
                reuse_local: false,
            },
        )))
        .await;
    match response.result? {
        ServerResponse::Repository(RepositoryResponse::SessionRepositoryAttached(repository)) => {
            Ok(repository)
        }
        response => Err(unexpected_response("repository attachment", response)),
    }
}

#[cfg(not(target_family = "wasm"))]
pub(crate) fn list_models(connection: &ClientConnection) -> Result<ModelCatalog, LoomError> {
    let response = connection.request(RequestEnvelope::new(ClientRequest::Provider(
        ProviderRequest::ListModels,
    )));
    match response.result? {
        ServerResponse::Provider(ProviderResponse::Models { models }) => {
            Ok(model_catalog_from_descriptors(models))
        }
        response => Err(unexpected_response("model list", response)),
    }
}

#[cfg(target_family = "wasm")]
pub(crate) async fn list_models_async(
    connection: &ClientConnection,
) -> Result<ModelCatalog, LoomError> {
    let response = connection
        .request(RequestEnvelope::new(ClientRequest::Provider(
            ProviderRequest::ListModels,
        )))
        .await;
    match response.result? {
        ServerResponse::Provider(ProviderResponse::Models { models }) => {
            Ok(model_catalog_from_descriptors(models))
        }
        response => Err(unexpected_response("model list", response)),
    }
}

pub(crate) struct ModelCatalog {
    pub models: Vec<ModelId>,
    pub provider_names: BTreeMap<ModelId, String>,
    pub discovery_errors: Vec<ModelDiscoveryError>,
}

fn model_catalog_from_descriptors(descriptors: Vec<loom_model::ModelDescriptor>) -> ModelCatalog {
    let provider_names = descriptors
        .iter()
        .map(|descriptor| {
            (
                ModelId::for_provider(&descriptor.provider, &descriptor.id),
                provider_name_for_id(descriptor.provider.as_str()),
            )
        })
        .collect();
    let models = descriptors
        .into_iter()
        .map(|descriptor| ModelId::for_provider(&descriptor.provider, &descriptor.id))
        .collect();
    ModelCatalog {
        models,
        provider_names,
        discovery_errors: Vec::new(),
    }
}

pub(crate) fn provider_name_for_id(provider_id: &str) -> String {
    match provider_id {
        "openai" => "OpenAI".to_owned(),
        "deepseek" => "DeepSeek".to_owned(),
        "github-copilot" => "GitHub Copilot".to_owned(),
        "ollama" => "Ollama".to_owned(),
        "deterministic" => "Demo".to_owned(),
        provider_id => provider_id
            .split(['-', '_'])
            .map(|part| {
                let mut chars = part.chars();
                chars
                    .next()
                    .map(|first| first.to_uppercase().chain(chars).collect::<String>())
                    .unwrap_or_default()
            })
            .collect::<Vec<_>>()
            .join(" "),
    }
}

pub(crate) struct ModelDiscoveryError {
    pub provider_id: String,
    pub error: LoomError,
}

fn include_discovered_models(
    provider_id: &str,
    provider_name: &str,
    result: Result<ServerResponse, LoomError>,
    models: &mut Vec<ModelId>,
    provider_names: &mut BTreeMap<ModelId, String>,
    discovery_errors: &mut Vec<ModelDiscoveryError>,
) {
    match result {
        Ok(ServerResponse::Provider(ProviderResponse::Models { models: discovered })) => {
            for model in discovered {
                let id = ModelId::for_provider(&ProviderId::new(provider_id), &model.id);
                provider_names.insert(id.clone(), provider_name.to_owned());
                models.push(id);
            }
        }
        Err(error) => discovery_errors.push(ModelDiscoveryError {
            provider_id: provider_id.to_owned(),
            error,
        }),
        Ok(response) => discovery_errors.push(ModelDiscoveryError {
            provider_id: provider_id.to_owned(),
            error: unexpected_response("model discovery", response),
        }),
    }
}

pub(crate) async fn list_models_from_backend(
    backend: &BackendWorker,
) -> Result<ModelCatalog, LoomError> {
    let response = backend
        .submit(RequestEnvelope::new(ClientRequest::Provider(
            ProviderRequest::ListProviders,
        )))
        .wait()
        .await;
    let mut providers = match response.result? {
        ServerResponse::Provider(ProviderResponse::Providers { providers }) => providers,
        response => return Err(unexpected_response("provider list", response)),
    };
    // Providers that advertise a seed model before any credential is stored
    // (the official OpenAI and DeepSeek APIs) must not contribute to the model
    // catalog or trigger discovery until they are usable.
    providers.retain(|provider: &ProviderSummary| provider.is_usable());
    let mut models = providers
        .iter()
        .flat_map(|provider| {
            provider
                .models
                .iter()
                .map(|model| ModelId::for_provider(&provider.id, &model.id))
        })
        .collect::<Vec<_>>();
    let mut provider_names = providers
        .iter()
        .flat_map(|provider| {
            provider.models.iter().map(|model| {
                (
                    ModelId::for_provider(&provider.id, &model.id),
                    provider.display_name.clone(),
                )
            })
        })
        .collect::<BTreeMap<_, _>>();
    let mut discovery_errors = Vec::new();
    for provider in providers {
        let provider_id = provider.id.as_str().to_owned();
        let provider_name = provider.display_name;
        let response = backend
            .submit(RequestEnvelope::new(ClientRequest::Provider(
                ProviderRequest::DiscoverProviderModels {
                    provider_id: provider.id.clone(),
                },
            )))
            .wait()
            .await;
        include_discovered_models(
            &provider_id,
            &provider_name,
            response.result,
            &mut models,
            &mut provider_names,
            &mut discovery_errors,
        );
    }
    models.sort();
    models.dedup();
    Ok(ModelCatalog {
        models,
        provider_names,
        discovery_errors,
    })
}

#[cfg(not(target_family = "wasm"))]
pub(crate) fn start_run(
    connection: &ClientConnection,
    session: &AgentSessionSnapshot,
    model: &ModelId,
    task: &str,
) -> Result<AgentRunSnapshot, LoomError> {
    let response = connection.request(RequestEnvelope::new(ClientRequest::Run(
        RunRequest::StartSessionAgentRun {
            session_id: session.id,
            task: task.to_owned(),
            model: model.clone(),
            system_instructions: Some(
                "Work methodically, use the available tools, and report validation.".to_owned(),
            ),
            repository_instructions: Some(
                "Keep the change focused and provide reviewable evidence.".to_owned(),
            ),
        },
    )));
    match response.result? {
        ServerResponse::Run(RunResponse::AgentRunStarted(run)) => Ok(run),
        response => Err(unexpected_response("agent run start", response)),
    }
}

/// Normalizes a response the client did not expect for the request it sent.
pub(crate) fn unexpected_response(operation: &str, response: ServerResponse) -> LoomError {
    LoomError::new(
        ErrorCode::Internal,
        format!("backend returned unexpected {operation} response: {response:?}"),
        false,
    )
}

#[cfg(all(test, not(target_family = "wasm")))]
mod tests {
    use super::{
        ClientConnection, ClientRequest, LoomError, NATIVE_ONLY_NEGOTIATION_CAPABILITIES,
        RequestEnvelope, SHARED_NEGOTIATION_CAPABILITIES, WorkspaceRequest, browser_subprotocols,
        create_session_in_workspace, create_workspace, describe_startup_connection_error,
        include_discovered_models, list_models, list_workspace_sessions, list_workspaces,
        negotiate, negotiation_capabilities, provider_name_for_id, redact_secret,
        remote_url_is_secure_for_secrets, resume_github_identity, set_workspace_config,
        unexpected_response, worker_node_status, workspace_config,
    };
    use loom_core::{
        AgentSessionId, Capability, CapabilitySet, ErrorCode, LoomError as CoreLoomError,
        Timestamp, WorkspaceId, WorkspaceRecord,
    };
    use loom_model::ModelId;
    use loom_protocol::{
        AccessRequest, AccessRequestId, CollaborationRequest, ParticipantId, ParticipantIdentity,
        ProviderResponse, RepositoryResponse, ServerResponse, SessionRole, WorkspaceConfig,
    };
    use loom_server::{AuthTokenStore, AuthorizationScope, RemoteServer, RemoteServerConfig};
    use std::sync::Arc;

    #[test]
    fn current_client_contract_negotiates_transcript_paging() {
        assert!(negotiation_capabilities().contains(Capability::ReadAgentRunMessages));
    }

    #[test]
    fn current_client_contract_negotiates_usage_and_context_inspection() {
        let capabilities = negotiation_capabilities();
        assert!(capabilities.contains(Capability::ReadUsage));
        assert!(capabilities.contains(Capability::InspectContext));
    }

    #[test]
    fn current_client_contract_negotiates_github_sign_in() {
        assert!(negotiation_capabilities().contains(Capability::GitHubSignIn));
    }

    #[test]
    fn a_worker_that_offers_sign_in_negotiates_it_with_the_client() {
        let connection = ClientConnection::InProcess(Box::new(
            crate::local_backend::OwnedBackend::new().connect(),
        ));
        let negotiation = negotiate(&connection).unwrap();
        assert!(negotiation.capabilities.contains(Capability::GitHubSignIn));
    }

    /// Every request the UI constructs must be able to negotiate the capability
    /// the worker gates it on. A request whose capability the client never asks
    /// for works only while it is not checked before dispatch; once it is, the
    /// worker answers `CapabilityDenied` and the action is impossible. This
    /// covers the access, collaboration and sign-in requests that regressed
    /// that way.
    ///
    /// `ProjectRequest::SendProjectAgentMessage` is deliberately absent: the UI
    /// never constructs it (the worker's project tool sends it in-process), so
    /// the client does not negotiate `SendProjectAgentMessage`.
    #[test]
    fn every_request_the_ui_sends_negotiates_its_required_capability() {
        let session_id = AgentSessionId::new();
        let participant_id = ParticipantId::new();
        let access_request_id = AccessRequestId::new();
        let requests = [
            ClientRequest::Access(AccessRequest::GetSessionRoster { session_id }),
            ClientRequest::Access(AccessRequest::ListParticipants),
            ClientRequest::Access(AccessRequest::GrantSessionAccess {
                session_id,
                participant_id,
            }),
            ClientRequest::Access(AccessRequest::SetSessionRole {
                session_id,
                participant_id,
                role: SessionRole::Participant,
            }),
            ClientRequest::Access(AccessRequest::RevokeSessionAccess {
                session_id,
                participant_id,
            }),
            ClientRequest::Access(AccessRequest::ListSessionAccessRequests { session_id }),
            ClientRequest::Access(AccessRequest::ApproveSessionAccessRequest {
                session_id,
                request_id: access_request_id,
                role: None,
            }),
            ClientRequest::Access(AccessRequest::DenySessionAccessRequest {
                session_id,
                request_id: access_request_id,
                reason: None,
            }),
            ClientRequest::Access(AccessRequest::RequestSessionAccess {
                session_id,
                requested_role: SessionRole::Participant,
                note: None,
            }),
            ClientRequest::Access(AccessRequest::WithdrawSessionAccessRequest {
                session_id,
                request_id: access_request_id,
            }),
            ClientRequest::Access(AccessRequest::StartGitHubSignIn),
            ClientRequest::Access(AccessRequest::ResumeGitHubIdentity { participant_id }),
            ClientRequest::Collaboration(CollaborationRequest::SendPeerMessage {
                session_id,
                text: "hello".to_owned(),
            }),
        ];
        let negotiated = negotiation_capabilities();
        for request in requests {
            let Some(required) = request.required_capability() else {
                continue;
            };
            assert!(
                negotiated.contains(required),
                "the UI sends {request:?}, which the worker gates on {required:?}, but the client \
                 does not negotiate {required:?}"
            );
        }
    }

    #[test]
    fn a_worker_that_offers_access_management_negotiates_it_with_the_client() {
        let connection = ClientConnection::InProcess(Box::new(
            crate::local_backend::OwnedBackend::new().connect(),
        ));
        let negotiation = negotiate(&connection).unwrap();
        assert!(
            negotiation
                .capabilities
                .contains(Capability::ReadSessionRoster)
        );
        assert!(
            negotiation
                .capabilities
                .contains(Capability::ManageSessionAccess)
        );
    }

    #[test]
    fn a_remembered_identity_resumes_on_an_operator_connection() {
        let backend = loom_server::InProcessBackend::new();
        let octocat = ParticipantIdentity {
            id: ParticipantId::new(),
            display_name: "octocat".to_owned(),
        };
        backend
            .register_participants(std::slice::from_ref(&octocat))
            .unwrap();
        let connection = ClientConnection::InProcess(Box::new(backend.connect()));
        negotiate(&connection).unwrap();

        assert_eq!(
            resume_github_identity(&connection, octocat.id).unwrap(),
            Some(octocat)
        );
        assert_eq!(
            resume_github_identity(&connection, ParticipantId::new())
                .unwrap_err()
                .code,
            ErrorCode::Conflict,
            "a connection already bound to the remembered person cannot resume again"
        );
    }

    #[test]
    fn capability_lists_agree_except_for_documented_transport_differences() {
        let native = negotiation_capabilities();
        // What the browser client advertises: the shared list alone.
        let wasm = CapabilitySet::new(SHARED_NEGOTIATION_CAPABILITIES.iter().copied());

        let mut documented_native_only = Vec::new();
        for (capability, reason) in NATIVE_ONLY_NEGOTIATION_CAPABILITIES {
            assert!(
                !reason.trim().is_empty(),
                "every native-only capability must document why the browser omits it"
            );
            assert!(
                !wasm.contains(*capability),
                "{capability:?} is documented as native-only but is also negotiated by the \
                 browser client"
            );
            assert!(
                native.contains(*capability),
                "{capability:?} is documented as native-only but the native client omits it"
            );
            documented_native_only.push(*capability);
        }

        // The transports agree on everything except the documented entries.
        assert_eq!(
            CapabilitySet::new(
                SHARED_NEGOTIATION_CAPABILITIES
                    .iter()
                    .chain(documented_native_only.iter())
                    .copied()
            ),
            native
        );
        // A duplicated entry would hide a capability the other transport omits.
        let unique = SHARED_NEGOTIATION_CAPABILITIES
            .iter()
            .copied()
            .collect::<std::collections::BTreeSet<_>>();
        assert_eq!(unique.len(), SHARED_NEGOTIATION_CAPABILITIES.len());
        assert_eq!(unique.len(), wasm.iter().count());
    }

    #[test]
    fn an_empty_token_offers_no_bearer_subprotocol() {
        assert!(browser_subprotocols("").is_empty());
        assert_eq!(
            browser_subprotocols("browser-secret"),
            vec!["loom.bearer.browser-secret".to_owned()]
        );
    }

    #[test]
    fn provider_secrets_require_tls_or_loopback_transport() {
        assert_eq!(
            remote_url_is_secure_for_secrets("wss://worker.example/ws"),
            super::native_transport_tls_supported()
        );
        assert!(remote_url_is_secure_for_secrets("ws://localhost:8080/ws"));
        assert!(remote_url_is_secure_for_secrets("ws://127.0.0.1:8080/ws"));
        assert!(remote_url_is_secure_for_secrets("ws://[::1]:8080/ws"));
        assert!(!remote_url_is_secure_for_secrets("ws://worker.example/ws"));
        assert!(!remote_url_is_secure_for_secrets(
            "ws://user:pass@localhost/ws"
        ));
        assert!(!remote_url_is_secure_for_secrets("wss:///ws"));
    }

    #[test]
    fn provider_names_use_branded_labels_and_pretty_fallbacks() {
        assert_eq!(provider_name_for_id("openai"), "OpenAI");
        assert_eq!(provider_name_for_id("deepseek"), "DeepSeek");
        assert_eq!(provider_name_for_id("github-copilot"), "GitHub Copilot");
        assert_eq!(provider_name_for_id("ollama"), "Ollama");
        assert_eq!(provider_name_for_id("deterministic"), "Demo");
        assert_eq!(
            provider_name_for_id("my_custom-provider"),
            "My Custom Provider"
        );
    }

    #[test]
    fn startup_error_messages_are_actionable_and_redact_credentials() {
        for (code, message, expected) in [
            (
                ErrorCode::AuthenticationFailed,
                "bad token",
                "worker authentication was denied",
            ),
            (
                ErrorCode::InvalidRequest,
                "invalid bearer token characters",
                "unsupported characters",
            ),
            (
                ErrorCode::InvalidRequest,
                "malformed endpoint",
                "invalid worker WebSocket URL",
            ),
            (ErrorCode::DeadlineExceeded, "timed out", "timed out"),
            (
                ErrorCode::ProviderUnavailable,
                "connection refused",
                "connection refused",
            ),
            (
                ErrorCode::UnsupportedProtocol,
                "version mismatch",
                "protocol negotiation failed",
            ),
            (
                ErrorCode::RequestCancelled,
                "closed",
                "closed the connection",
            ),
        ] {
            let error = CoreLoomError::new(code, message, false);
            assert!(describe_startup_connection_error(&error, "secret-token").contains(expected));
        }
        let raw = CoreLoomError::new(ErrorCode::Internal, "failed secret-token", false);
        let described = describe_startup_connection_error(&raw, "secret-token");
        assert!(!described.contains("secret-token"));
        assert_eq!(redact_secret("?token=a%2Fb", "a/b"), "?token=[redacted]");
        assert_eq!(redact_secret("unchanged", ""), "unchanged");
        assert!(
            unexpected_response(
                "probe",
                ServerResponse::Repository(RepositoryResponse::SessionRepositoryDetached)
            )
            .message
            .contains("probe")
        );
    }

    #[test]
    fn in_process_connection_exercises_workspace_and_session_client_operations() {
        let connection = ClientConnection::InProcess(Box::new(
            crate::local_backend::OwnedBackend::new().connect(),
        ));
        negotiate(&connection).unwrap();
        let created = create_workspace(&connection, "client wrapper test").unwrap();
        assert_eq!(created.name, "client wrapper test");
        assert!(
            list_workspaces(&connection)
                .unwrap()
                .iter()
                .any(|workspace| workspace.id == created.id)
        );

        let config = WorkspaceConfig {
            revision: 1,
            cpu_pulse_threshold_percent: 12,
            ..WorkspaceConfig::default()
        };
        set_workspace_config(&connection, created.id, config.clone()).unwrap();
        assert_eq!(workspace_config(&connection, created.id).unwrap(), config);

        let session =
            create_session_in_workspace(&connection, created.id, "client session").unwrap();
        assert_eq!(session.name, "client session");
        assert_eq!(
            list_workspace_sessions(&connection, created.id).unwrap(),
            vec![session.clone()]
        );
        assert!(!list_models(&connection).unwrap().models.is_empty());
    }

    #[test]
    fn workspace_client_operations_report_backend_errors_and_register_external_records() {
        let connection = ClientConnection::InProcess(Box::new(
            crate::local_backend::OwnedBackend::new().connect(),
        ));
        negotiate(&connection).unwrap();
        let timestamp = Timestamp::from_unix_millis(1);
        let external = WorkspaceRecord {
            id: WorkspaceId::new(),
            name: "registered externally".to_owned(),
            created_at: timestamp,
            updated_at: timestamp,
        };
        let registered = connection.request(RequestEnvelope::new(ClientRequest::Workspace(
            WorkspaceRequest::RegisterWorkspace {
                workspace: external.clone(),
            },
        )));
        assert!(registered.result.is_ok());
        assert!(
            list_workspaces(&connection)
                .unwrap()
                .iter()
                .any(|workspace| workspace == &external)
        );
    }

    #[test]
    fn model_catalogs_preserve_same_model_from_distinct_providers() {
        let descriptor = |provider: &str| loom_model::ModelDescriptor {
            id: ModelId::new("gpt-5"),
            provider: loom_model::ProviderId::new(provider),
            display_name: "GPT-5".to_owned(),
            context_window: None,
            max_input_tokens: None,
            max_output_tokens: None,
            capabilities: loom_model::ModelCapabilities::default(),
        };
        let mut catalog = super::model_catalog_from_descriptors(vec![
            descriptor("openai"),
            descriptor("github-copilot"),
        ]);
        assert_eq!(catalog.models.len(), 2);
        assert_ne!(catalog.models[0], catalog.models[1]);
        assert_eq!(catalog.provider_names[&catalog.models[0]], "OpenAI");
        assert_eq!(catalog.provider_names[&catalog.models[1]], "GitHub Copilot");
        for (provider, name) in [
            ("openai", "Worker OpenAI"),
            ("github-copilot", "Worker Copilot"),
        ] {
            include_discovered_models(
                provider,
                name,
                Ok(ServerResponse::Provider(ProviderResponse::Models {
                    models: vec![descriptor(provider)],
                })),
                &mut catalog.models,
                &mut catalog.provider_names,
                &mut catalog.discovery_errors,
            );
        }
        catalog.models.sort();
        catalog.models.dedup();
        assert_eq!(catalog.models.len(), 2);
        for model in &catalog.models {
            let (provider, raw) = model.provider_model().unwrap();
            assert_eq!(raw, ModelId::new("gpt-5"));
            let expected = if provider.as_str() == "openai" {
                "Worker OpenAI"
            } else {
                "Worker Copilot"
            };
            assert_eq!(catalog.provider_names[model], expected);
        }
        assert!(catalog.discovery_errors.is_empty());
    }

    #[test]
    fn provider_discovery_failure_preserves_static_models() {
        let copilot_model = ModelId::new("github-copilot/gpt-5.6-luna");
        let mut models = vec![copilot_model.clone()];
        let mut discovery_errors = Vec::new();

        include_discovered_models(
            "ollama",
            "Ollama",
            Err(LoomError::new(
                ErrorCode::ProviderUnavailable,
                "Ollama is unavailable",
                true,
            )),
            &mut models,
            &mut std::collections::BTreeMap::new(),
            &mut discovery_errors,
        );

        assert_eq!(models, vec![copilot_model]);
        assert_eq!(discovery_errors.len(), 1);
        assert_eq!(discovery_errors[0].provider_id, "ollama");
        assert_eq!(
            discovery_errors[0].error.code,
            ErrorCode::ProviderUnavailable
        );
    }

    #[gpui_kit::test]
    async fn native_remote_connection_works_on_the_gpui_background_executor(
        cx: &mut gpui_kit::TestAppContext,
    ) {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("test server runtime should start");
        let backend = loom_server::InProcessBackend::new();
        let auth = Arc::new(AuthTokenStore::new());
        let token = auth
            .insert("gpui-background-test", AuthorizationScope::all())
            .expect("test credential should be issued");
        let server = runtime
            .block_on(
                RemoteServer::new(backend, auth, RemoteServerConfig::local_ephemeral()).bind(),
            )
            .expect("test server should bind");
        let url = server.websocket_url().to_owned();

        let invalid_url = cx
            .background_executor
            .spawn(async {
                ClientConnection::remote(
                    "not a websocket URL".to_owned(),
                    "test-token".to_owned(),
                    &loom_local::RemoteConnectionOptions::default(),
                )
            })
            .await;
        assert!(matches!(
            invalid_url,
            Err(error) if error.code == ErrorCode::InvalidRequest
        ));

        let connected_status = cx
            .background_executor
            .spawn(async move {
                let connection = ClientConnection::remote(
                    url,
                    token.token,
                    &loom_local::RemoteConnectionOptions::default(),
                )?;
                negotiate(&connection)?;
                let status = worker_node_status(&connection)?;
                connection.close()?;
                Ok::<_, LoomError>(status)
            })
            .await
            .expect("remote connect, negotiation, and status should succeed");
        assert!(connected_status.online);

        runtime
            .block_on(server.stop())
            .expect("test server should stop cleanly");
    }
}
