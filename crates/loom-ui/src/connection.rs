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
    AgentSessionSnapshot, Capability, CapabilitySet, ErrorCode, LoomError, WorkspaceId,
    WorkspaceRecord,
};
use loom_model::{ModelId, ProviderSummary};
#[cfg(not(target_family = "wasm"))]
use loom_protocol::AgentRunSnapshot;
use loom_protocol::{
    CURRENT_PROTOCOL_VERSION, ClientRequest, ControlRequest, ControlResponse, NegotiationResult,
    ProviderRequest, ProviderResponse, RepositoryRequest, RepositoryResponse, RequestEnvelope,
    ResponseEnvelope, ServerResponse, SessionRepository, SessionResponse, WorkerNodeStatus,
    WorkspaceConfig, WorkspaceRequest, WorkspaceResponse,
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
        return true;
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
    #[cfg(not(target_family = "wasm"))]
    InProcess(Box<loom_local::LocalConnection>),
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
    pub(crate) fn remote(url: String, token: String) -> Result<Self, LoomError> {
        let secure_for_secrets = remote_url_is_secure_for_secrets(&url);
        Ok(Self::Remote(loom_local::RemoteConnection::connect(
            &url,
            &token,
            secure_for_secrets,
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
            #[cfg(not(target_family = "wasm"))]
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
    /// bootstrap, before any window exists.
    #[cfg(not(target_family = "wasm"))]
    pub(crate) fn request(&self, request: RequestEnvelope) -> ResponseEnvelope {
        match self {
            Self::InProcess(connection) => connection.request(request),
            Self::Remote(connection) => connection.request(request),
        }
    }

    #[cfg(not(target_family = "wasm"))]
    pub(crate) fn request_with_timeout(
        &self,
        request: RequestEnvelope,
        timeout: Duration,
    ) -> ResponseEnvelope {
        match self {
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

#[cfg(not(target_family = "wasm"))]
pub(crate) fn negotiation_capabilities() -> CapabilitySet {
    CapabilitySet::new([
        Capability::CreateAgentSession,
        Capability::ReadAgentSession,
        Capability::ControlAgentSession,
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
        Capability::ConfigureApprovalPolicy,
        Capability::ManageCheckpoints,
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
        Capability::JsonProtocol,
    ])
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

#[cfg(target_family = "wasm")]
pub(crate) fn negotiation_capabilities() -> CapabilitySet {
    CapabilitySet::new([
        Capability::CreateAgentSession,
        Capability::ReadAgentSession,
        Capability::ControlAgentSession,
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
        Capability::ConfigureApprovalPolicy,
        Capability::ManageWorkspaces,
        Capability::ManageSessionRepositories,
        Capability::ReadSessionFilesystem,
        Capability::WriteSessionFilesystem,
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
        Capability::JsonProtocol,
    ])
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
    )
    .await?;
    match response.result? {
        ServerResponse::Control(ControlResponse::WorkerNodeStatus(status)) => Ok(status),
        response => Err(unexpected_response("worker node status", response)),
    }
}

#[cfg(target_family = "wasm")]
async fn request_with_timeout(
    connection: &ClientConnection,
    request: RequestEnvelope,
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
            15_000,
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
pub(crate) fn register_workspace(
    connection: &ClientConnection,
    workspace: WorkspaceRecord,
) -> Result<(), LoomError> {
    let response = connection.request(RequestEnvelope::new(ClientRequest::Workspace(
        WorkspaceRequest::RegisterWorkspace { workspace },
    )));
    match response.result? {
        ServerResponse::Workspace(WorkspaceResponse::WorkspaceCreated(_)) => Ok(()),
        response => Err(unexpected_response("workspace registration", response)),
    }
}

#[cfg(target_family = "wasm")]
pub(crate) async fn register_workspace_async(
    connection: &ClientConnection,
    workspace: WorkspaceRecord,
) -> Result<(), LoomError> {
    let response = connection
        .request(RequestEnvelope::new(ClientRequest::Workspace(
            WorkspaceRequest::RegisterWorkspace { workspace },
        )))
        .await;
    match response.result? {
        ServerResponse::Workspace(WorkspaceResponse::WorkspaceCreated(_)) => Ok(()),
        response => Err(unexpected_response("workspace registration", response)),
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
                descriptor.id.clone(),
                provider_name_for_id(descriptor.provider.as_str()),
            )
        })
        .collect();
    let models = descriptors
        .into_iter()
        .map(|descriptor| descriptor.id)
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
                provider_names.insert(model.id.clone(), provider_name.to_owned());
                models.push(model.id);
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
        .flat_map(|provider| provider.models.iter().map(|model| model.id.clone()))
        .collect::<Vec<_>>();
    let mut provider_names = providers
        .iter()
        .flat_map(|provider| {
            provider
                .models
                .iter()
                .map(|model| (model.id.clone(), provider.display_name.clone()))
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
        ClientConnection, LoomError, create_session_in_workspace, create_workspace,
        describe_startup_connection_error, include_discovered_models, list_models,
        list_workspace_sessions, list_workspaces, negotiate, negotiation_capabilities,
        provider_name_for_id, redact_secret, register_workspace, remote_url_is_secure_for_secrets,
        set_workspace_config, unexpected_response, worker_node_status, workspace_config,
    };
    use loom_core::{
        Capability, ErrorCode, LoomError as CoreLoomError, Timestamp, WorkspaceId, WorkspaceRecord,
    };
    use loom_model::ModelId;
    use loom_protocol::{RepositoryResponse, ServerResponse, WorkspaceConfig};
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
    fn provider_secrets_require_tls_or_loopback_transport() {
        assert!(remote_url_is_secure_for_secrets("wss://worker.example/ws"));
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
        let connection =
            ClientConnection::InProcess(Box::new(loom_local::OwnedBackend::new().connect()));
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
        let connection =
            ClientConnection::InProcess(Box::new(loom_local::OwnedBackend::new().connect()));
        negotiate(&connection).unwrap();
        let timestamp = Timestamp::from_unix_millis(1);
        let external = WorkspaceRecord {
            id: WorkspaceId::new(),
            name: "registered externally".to_owned(),
            created_at: timestamp,
            updated_at: timestamp,
        };
        register_workspace(&connection, external.clone()).unwrap();
        assert!(
            list_workspaces(&connection)
                .unwrap()
                .iter()
                .any(|workspace| workspace == &external)
        );
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
                ClientConnection::remote("not a websocket URL".to_owned(), "test-token".to_owned())
            })
            .await;
        assert!(matches!(
            invalid_url,
            Err(error) if error.code == ErrorCode::InvalidRequest
        ));

        let connected_status = cx
            .background_executor
            .spawn(async move {
                let connection = ClientConnection::remote(url, token.token)?;
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
