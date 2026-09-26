//! Transport-independent protocol client for the native shell and browser.
//!
//! Native builds talk to either an in-process backend or a remote backend
//! over a native WebSocket (via `loom-server`'s blocking client). The browser
//! build only ever talks to a remote backend, over a real `web_sys::WebSocket`
//! (see `browser::BrowserConnection`), since it cannot spawn an in-process
//! backend or open a native socket. Both variants share one request/response
//! shape so the UI (`view.rs`) never needs to know which transport it's
//! using.

#[cfg(not(target_family = "wasm"))]
use std::sync::{Arc, Mutex};

#[cfg(not(target_family = "wasm"))]
use std::{
    sync::mpsc::{self, Sender},
    time::Duration,
};

#[cfg(all(not(target_family = "wasm"), not(test)))]
use std::thread;

use futures_channel::oneshot;
use loom_core::{
    AgentSessionSnapshot, Capability, CapabilitySet, ErrorCode, LoomError, RequestId, WorkspaceId,
    WorkspaceRecord,
};
use loom_model::ModelId;
#[cfg(not(target_family = "wasm"))]
use loom_model::ProviderId;
#[cfg(not(target_family = "wasm"))]
use loom_protocol::AgentRunSnapshot;
use loom_protocol::{
    CURRENT_PROTOCOL_VERSION, ClientRequest, RequestEnvelope, ResponseEnvelope, ServerResponse,
    SessionRepository, WorkerNodeStatus, WorkspaceConfig,
};
#[cfg(target_family = "wasm")]
use std::task::Poll;

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

#[cfg(not(target_family = "wasm"))]
fn remote_url_is_secure_for_secrets(url: &str) -> bool {
    if url.starts_with("wss://") {
        return true;
    }
    let Some(authority) = url
        .strip_prefix("ws://")
        .map(|url| url.split(['/', '?', '#']).next().unwrap_or_default())
    else {
        return false;
    };
    let host = if let Some(bracketed_host) = authority.strip_prefix('[') {
        bracketed_host.split(']').next().unwrap_or_default()
    } else {
        authority
            .rsplit_once(':')
            .map_or(authority, |(host, _)| host)
    };
    matches!(host, "localhost" | "127.0.0.1" | "::1")
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

#[cfg(not(target_family = "wasm"))]
use loom_server::{InProcessConnection, WebSocketConnection, WebSocketTransport};

#[cfg(target_family = "wasm")]
use crate::browser::BrowserConnection;
#[cfg(target_family = "wasm")]
use wasm_bindgen::{JsCast, closure::Closure};

#[derive(Clone)]
pub(crate) enum ClientConnection {
    #[cfg(not(target_family = "wasm"))]
    InProcess(Box<InProcessConnection>),
    #[cfg(not(target_family = "wasm"))]
    Remote {
        runtime: Arc<tokio::runtime::Runtime>,
        connection: Arc<Mutex<WebSocketConnection>>,
        secure_for_secrets: bool,
    },
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
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|error| {
                LoomError::new(
                    ErrorCode::Internal,
                    format!("could not create remote client runtime: {error}"),
                    false,
                )
            })?;
        let connection = runtime.block_on(async {
            tokio::time::timeout(
                Duration::from_secs(15),
                WebSocketTransport::new(&url, &token).connect(),
            )
            .await
        });
        let connection = match connection {
            Ok(result) => result?,
            Err(_) => {
                return Err(LoomError::new(
                    ErrorCode::DeadlineExceeded,
                    "worker connection timed out",
                    true,
                ));
            }
        };
        Ok(Self::Remote {
            runtime: Arc::new(runtime),
            connection: Arc::new(Mutex::new(connection)),
            secure_for_secrets,
        })
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
                Self::Remote {
                    runtime,
                    connection,
                    ..
                } => {
                    let mut connection = connection.lock().map_err(|_| {
                        LoomError::new(
                            ErrorCode::Internal,
                            "remote connection lock was poisoned",
                            true,
                        )
                    })?;
                    runtime.block_on(connection.close())
                }
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
            Self::Remote {
                secure_for_secrets, ..
            } => *secure_for_secrets,
            #[cfg(target_family = "wasm")]
            Self::Disconnected => false,
            #[cfg(target_family = "wasm")]
            Self::Browser(_) => false,
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
            Self::Remote {
                runtime,
                connection,
                ..
            } => {
                let request_id = request.request_id;
                let result = connection
                    .lock()
                    .map_err(|_| {
                        LoomError::new(
                            ErrorCode::Internal,
                            "remote connection lock was poisoned",
                            true,
                        )
                    })
                    .and_then(|mut connection| runtime.block_on(connection.request(request)));
                match result {
                    Ok(response) => response,
                    Err(error) => ResponseEnvelope::failure(request_id, error),
                }
            }
        }
    }

    #[cfg(not(target_family = "wasm"))]
    pub(crate) fn request_with_timeout(
        &self,
        request: RequestEnvelope,
        timeout: Duration,
    ) -> ResponseEnvelope {
        let request_id = request.request_id;
        match self {
            Self::InProcess(connection) => connection.request(request),
            Self::Remote {
                runtime,
                connection,
                ..
            } => {
                let result = connection
                    .lock()
                    .map_err(|_| {
                        LoomError::new(
                            ErrorCode::Internal,
                            "remote connection lock was poisoned",
                            true,
                        )
                    })
                    .and_then(|mut connection| {
                        runtime.block_on(async {
                            match tokio::time::timeout(timeout, connection.request(request)).await {
                                Ok(result) => result,
                                Err(_) => Err(LoomError::new(
                                    ErrorCode::DeadlineExceeded,
                                    "worker request timed out",
                                    true,
                                )),
                            }
                        })
                    });
                result.unwrap_or_else(|error| ResponseEnvelope::failure(request_id, error))
            }
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
pub(crate) fn negotiate(connection: &ClientConnection) -> Result<(), LoomError> {
    let response = connection.request_with_timeout(
        RequestEnvelope::new(ClientRequest::Negotiate {
            client_version: CURRENT_PROTOCOL_VERSION,
            capabilities: CapabilitySet::new([
                Capability::CreateAgentSession,
                Capability::ReadAgentSession,
                Capability::ControlAgentSession,
                Capability::ManageWorkspaces,
                Capability::ManageSessionRepositories,
                Capability::ReadSessionFilesystem,
                Capability::WriteSessionFilesystem,
                Capability::SubscribeSessionEvents,
                Capability::StartAgentRun,
                Capability::ReadAgentRun,
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
                Capability::ManageWorkspaces,
                Capability::ManageSessionRepositories,
                Capability::ReadSessionFilesystem,
                Capability::WriteSessionFilesystem,
                Capability::ReadVcsStatus,
                Capability::ReadVcsDiff,
                Capability::ReadSessionTask,
                Capability::StartSessionTask,
                Capability::ControlSessionTask,
                Capability::ReadSessionTaskEvidence,
                Capability::ReadWorkerNodeStatus,
                Capability::JsonProtocol,
            ]),
        }),
        Duration::from_secs(15),
    );
    match response.result? {
        ServerResponse::Negotiated(_) => Ok(()),
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
        Capability::StartAgentRun,
        Capability::ReadAgentRun,
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
        Capability::ReadSessionTask,
        Capability::StartSessionTask,
        Capability::ControlSessionTask,
        Capability::ReadSessionTaskEvidence,
        Capability::ReadWorkerNodeStatus,
        Capability::JsonProtocol,
    ])
}

#[cfg(not(target_family = "wasm"))]
pub(crate) fn worker_node_status(
    connection: &ClientConnection,
) -> Result<WorkerNodeStatus, LoomError> {
    let response = connection.request_with_timeout(
        RequestEnvelope::new(ClientRequest::GetWorkerNodeStatus),
        Duration::from_secs(15),
    );
    match response.result? {
        ServerResponse::WorkerNodeStatus(status) => Ok(status),
        response => Err(unexpected_response("worker node status", response)),
    }
}

#[cfg(target_family = "wasm")]
pub(crate) async fn worker_node_status_async(
    connection: &ClientConnection,
) -> Result<WorkerNodeStatus, LoomError> {
    let response = request_with_timeout(
        connection,
        RequestEnvelope::new(ClientRequest::GetWorkerNodeStatus),
    )
    .await?;
    match response.result? {
        ServerResponse::WorkerNodeStatus(status) => Ok(status),
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
    window
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
    timeout_callback.forget();
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
    let response = connection.request(RequestEnvelope::new(
        ClientRequest::GetWorkspaceConfigForWorkspace { workspace_id },
    ));
    match response.result? {
        ServerResponse::WorkspaceConfig(config) => Ok(config),
        response => Err(unexpected_response("workspace config", response)),
    }
}

#[cfg(target_family = "wasm")]
pub(crate) async fn workspace_config_async(
    connection: &ClientConnection,
    workspace_id: WorkspaceId,
) -> Result<WorkspaceConfig, LoomError> {
    let response = connection
        .request(RequestEnvelope::new(
            ClientRequest::GetWorkspaceConfigForWorkspace { workspace_id },
        ))
        .await;
    match response.result? {
        ServerResponse::WorkspaceConfig(config) => Ok(config),
        response => Err(unexpected_response("workspace config", response)),
    }
}

#[cfg(not(target_family = "wasm"))]
pub(crate) fn set_workspace_config(
    connection: &ClientConnection,
    workspace_id: WorkspaceId,
    config: WorkspaceConfig,
) -> Result<(), LoomError> {
    let response = connection.request(RequestEnvelope::new(
        ClientRequest::SetWorkspaceConfigForWorkspace {
            workspace_id,
            config,
        },
    ));
    match response.result? {
        ServerResponse::WorkspaceConfigUpdated => Ok(()),
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
        .request(RequestEnvelope::new(
            ClientRequest::SetWorkspaceConfigForWorkspace {
                workspace_id,
                config,
            },
        ))
        .await;
    match response.result? {
        ServerResponse::WorkspaceConfigUpdated => Ok(()),
        response => Err(unexpected_response("workspace config update", response)),
    }
}

#[cfg(target_family = "wasm")]
pub(crate) async fn negotiate_async(connection: &ClientConnection) -> Result<(), LoomError> {
    let response = request_with_timeout(
        connection,
        RequestEnvelope::new(ClientRequest::Negotiate {
            client_version: CURRENT_PROTOCOL_VERSION,
            capabilities: negotiation_capabilities(),
        }),
    )
    .await?;
    match response.result? {
        ServerResponse::Negotiated(_) => Ok(()),
        response => Err(unexpected_response("negotiation", response)),
    }
}

#[cfg(not(target_family = "wasm"))]
pub(crate) fn list_workspaces(
    connection: &ClientConnection,
) -> Result<Vec<WorkspaceRecord>, LoomError> {
    let response = connection.request(RequestEnvelope::new(ClientRequest::ListWorkspaces));
    match response.result? {
        ServerResponse::Workspaces { workspaces } => Ok(workspaces),
        response => Err(unexpected_response("workspace list", response)),
    }
}

#[cfg(target_family = "wasm")]
pub(crate) async fn list_workspaces_async(
    connection: &ClientConnection,
) -> Result<Vec<WorkspaceRecord>, LoomError> {
    let response = connection
        .request(RequestEnvelope::new(ClientRequest::ListWorkspaces))
        .await;
    match response.result? {
        ServerResponse::Workspaces { workspaces } => Ok(workspaces),
        response => Err(unexpected_response("workspace list", response)),
    }
}

#[cfg(not(target_family = "wasm"))]
pub(crate) fn create_workspace(
    connection: &ClientConnection,
    name: &str,
) -> Result<WorkspaceRecord, LoomError> {
    let response = connection.request(RequestEnvelope::new(ClientRequest::CreateWorkspace {
        name: name.to_owned(),
    }));
    match response.result? {
        ServerResponse::WorkspaceCreated(workspace) => Ok(workspace),
        response => Err(unexpected_response("workspace creation", response)),
    }
}

#[cfg(target_family = "wasm")]
pub(crate) async fn create_workspace_async(
    connection: &ClientConnection,
    name: &str,
) -> Result<WorkspaceRecord, LoomError> {
    let response = connection
        .request(RequestEnvelope::new(ClientRequest::CreateWorkspace {
            name: name.to_owned(),
        }))
        .await;
    match response.result? {
        ServerResponse::WorkspaceCreated(workspace) => Ok(workspace),
        response => Err(unexpected_response("workspace creation", response)),
    }
}

#[cfg(not(target_family = "wasm"))]
pub(crate) fn register_workspace(
    connection: &ClientConnection,
    workspace: WorkspaceRecord,
) -> Result<(), LoomError> {
    let response = connection.request(RequestEnvelope::new(ClientRequest::RegisterWorkspace {
        workspace,
    }));
    match response.result? {
        ServerResponse::WorkspaceCreated(_) => Ok(()),
        response => Err(unexpected_response("workspace registration", response)),
    }
}

#[cfg(target_family = "wasm")]
pub(crate) async fn register_workspace_async(
    connection: &ClientConnection,
    workspace: WorkspaceRecord,
) -> Result<(), LoomError> {
    let response = connection
        .request(RequestEnvelope::new(ClientRequest::RegisterWorkspace {
            workspace,
        }))
        .await;
    match response.result? {
        ServerResponse::WorkspaceCreated(_) => Ok(()),
        response => Err(unexpected_response("workspace registration", response)),
    }
}

#[cfg(not(target_family = "wasm"))]
pub(crate) fn list_workspace_sessions(
    connection: &ClientConnection,
    workspace_id: WorkspaceId,
) -> Result<Vec<AgentSessionSnapshot>, LoomError> {
    let response = connection.request(RequestEnvelope::new(ClientRequest::ListWorkspaceSessions {
        workspace_id,
        include_archived: false,
    }));
    match response.result? {
        ServerResponse::AgentSessions { sessions } => Ok(sessions),
        response => Err(unexpected_response("workspace session list", response)),
    }
}

#[cfg(target_family = "wasm")]
pub(crate) async fn list_workspace_sessions_async(
    connection: &ClientConnection,
    workspace_id: WorkspaceId,
) -> Result<Vec<AgentSessionSnapshot>, LoomError> {
    let response = connection
        .request(RequestEnvelope::new(ClientRequest::ListWorkspaceSessions {
            workspace_id,
            include_archived: false,
        }))
        .await;
    match response.result? {
        ServerResponse::AgentSessions { sessions } => Ok(sessions),
        response => Err(unexpected_response("workspace session list", response)),
    }
}

#[cfg(not(target_family = "wasm"))]
pub(crate) fn create_session_in_workspace(
    connection: &ClientConnection,
    workspace_id: WorkspaceId,
    name: &str,
) -> Result<AgentSessionSnapshot, LoomError> {
    let response = connection.request(RequestEnvelope::new(
        ClientRequest::CreateAgentSessionInWorkspace {
            workspace_id,
            name: name.to_owned(),
        },
    ));
    match response.result? {
        ServerResponse::AgentSessionCreated(snapshot) => Ok(snapshot),
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
        .request(RequestEnvelope::new(
            ClientRequest::CreateAgentSessionInWorkspace {
                workspace_id,
                name: name.to_owned(),
            },
        ))
        .await;
    match response.result? {
        ServerResponse::AgentSessionCreated(snapshot) => Ok(snapshot),
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
    let response = connection.request(RequestEnvelope::new(
        ClientRequest::AttachSessionRepository {
            session_id,
            source: source.to_owned(),
            path: path.to_owned(),
            revision: None,
        },
    ));
    match response.result? {
        ServerResponse::SessionRepositoryAttached(repository) => Ok(repository),
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
        .request(RequestEnvelope::new(
            ClientRequest::AttachSessionRepository {
                session_id,
                source: source.to_owned(),
                path: path.to_owned(),
                revision: None,
            },
        ))
        .await;
    match response.result? {
        ServerResponse::SessionRepositoryAttached(repository) => Ok(repository),
        response => Err(unexpected_response("repository attachment", response)),
    }
}

#[cfg(not(target_family = "wasm"))]
pub(crate) fn list_models(connection: &ClientConnection) -> Result<Vec<ModelId>, LoomError> {
    let response = connection.request(RequestEnvelope::new(ClientRequest::ListModels));
    match response.result? {
        ServerResponse::Models { models } => Ok(models.into_iter().map(|model| model.id).collect()),
        response => Err(unexpected_response("model list", response)),
    }
}

#[cfg(target_family = "wasm")]
pub(crate) async fn list_models_async(
    connection: &ClientConnection,
) -> Result<Vec<ModelId>, LoomError> {
    let response = connection
        .request(RequestEnvelope::new(ClientRequest::ListModels))
        .await;
    match response.result? {
        ServerResponse::Models { models } => Ok(models.into_iter().map(|model| model.id).collect()),
        response => Err(unexpected_response("model list", response)),
    }
}

pub(crate) struct ModelCatalog {
    pub models: Vec<ModelId>,
    pub discovery_errors: Vec<ModelDiscoveryError>,
}

pub(crate) struct ModelDiscoveryError {
    pub provider_id: String,
    pub error: LoomError,
}

fn include_discovered_models(
    provider_id: &str,
    result: Result<ServerResponse, LoomError>,
    models: &mut Vec<ModelId>,
    discovery_errors: &mut Vec<ModelDiscoveryError>,
) {
    match result {
        Ok(ServerResponse::Models { models: discovered }) => {
            models.extend(discovered.into_iter().map(|model| model.id))
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
        .submit(RequestEnvelope::new(ClientRequest::ListProviders))
        .wait()
        .await;
    let providers = match response.result? {
        ServerResponse::Providers { providers } => providers,
        response => return Err(unexpected_response("provider list", response)),
    };
    let mut models = providers
        .iter()
        .flat_map(|provider| provider.models.iter().map(|model| model.id.clone()))
        .collect::<Vec<_>>();
    let mut discovery_errors = Vec::new();
    for provider in providers {
        let provider_id = provider.id.as_str().to_owned();
        let response = backend
            .submit(RequestEnvelope::new(
                ClientRequest::DiscoverProviderModels {
                    provider_id: provider.id.clone(),
                },
            ))
            .wait()
            .await;
        include_discovered_models(
            &provider_id,
            response.result,
            &mut models,
            &mut discovery_errors,
        );
    }
    models.sort();
    models.dedup();
    Ok(ModelCatalog {
        models,
        discovery_errors,
    })
}

#[cfg(not(target_family = "wasm"))]
pub(crate) fn list_provider_ids(
    connection: &ClientConnection,
) -> Result<Vec<ProviderId>, LoomError> {
    let response = connection.request(RequestEnvelope::new(ClientRequest::ListProviders));
    match response.result? {
        ServerResponse::Providers { providers } => {
            Ok(providers.into_iter().map(|provider| provider.id).collect())
        }
        response => Err(unexpected_response("provider list", response)),
    }
}

#[cfg(not(target_family = "wasm"))]
pub(crate) fn start_run(
    connection: &ClientConnection,
    session: &AgentSessionSnapshot,
    model: &ModelId,
    task: &str,
) -> Result<AgentRunSnapshot, LoomError> {
    let response = connection.request(RequestEnvelope::new(ClientRequest::StartSessionAgentRun {
        session_id: session.id,
        task: task.to_owned(),
        model: model.clone(),
        system_instructions: Some(
            "Work methodically, use the available tools, and report validation.".to_owned(),
        ),
        repository_instructions: Some(
            "Keep the change focused and provide reviewable evidence.".to_owned(),
        ),
    }));
    match response.result? {
        ServerResponse::AgentRunStarted(run) => Ok(run),
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

/// A submitted request whose response has not arrived yet.
pub(crate) struct PendingResponse {
    request_id: RequestId,
    reply: oneshot::Receiver<ResponseEnvelope>,
}

impl PendingResponse {
    /// Awaits the worker's answer.
    ///
    /// Natively this runs on a background OS thread and the underlying
    /// worker call is a genuine blocking wait; in the browser it awaits a
    /// channel fed by the WebSocket's `onmessage` callback. Either way,
    /// callers just do `pending.wait().await`.
    pub(crate) async fn wait(self) -> ResponseEnvelope {
        self.reply.await.unwrap_or_else(|_| {
            ResponseEnvelope::failure(
                self.request_id,
                LoomError::new(
                    ErrorCode::Internal,
                    "the client connection worker stopped before answering",
                    false,
                ),
            )
        })
    }
}

/// Owns the protocol connection.
///
/// Natively, a dedicated worker thread executes requests in submission
/// order, which also keeps the remote transport's single in-flight request
/// contract; UI handlers submit a request and await a [`PendingResponse`] on
/// a background task, so a handler never blocks the GPUI thread on backend
/// latency. In the browser there is only one JS thread, so each submitted
/// request is instead driven forward as its own cooperative task on that
/// same thread; the browser transport already supports overlapping in-flight
/// requests (it correlates responses by request id), so no additional
/// serialization is needed there.
#[derive(Clone)]
pub(crate) struct BackendWorker {
    #[cfg(not(target_family = "wasm"))]
    jobs: Sender<Job>,
    #[cfg(all(not(target_family = "wasm"), test))]
    test_connection: Option<ClientConnection>,
    #[cfg(target_family = "wasm")]
    connection: ClientConnection,
    secure_for_secrets: bool,
}

#[cfg(not(target_family = "wasm"))]
#[allow(dead_code)]
struct Job {
    request: RequestEnvelope,
    reply: oneshot::Sender<ResponseEnvelope>,
}

impl BackendWorker {
    #[cfg(all(not(target_family = "wasm"), test))]
    pub(crate) fn spawn(connection: ClientConnection) -> Self {
        let secure_for_secrets = connection.secure_for_secrets();
        let (jobs, _incoming) = mpsc::channel::<Job>();
        Self {
            jobs,
            test_connection: Some(connection),
            secure_for_secrets,
        }
    }

    #[cfg(all(not(target_family = "wasm"), not(test)))]
    pub(crate) fn spawn(connection: ClientConnection) -> Self {
        let secure_for_secrets = connection.secure_for_secrets();
        let (jobs, incoming) = mpsc::channel::<Job>();
        thread::spawn(move || {
            while let Ok(job) = incoming.recv() {
                let response = connection.request(job.request);
                // A dropped receiver means the view stopped caring about this
                // request; the backend has already applied it either way.
                let _ = job.reply.send(response);
            }
        });
        Self {
            jobs,
            secure_for_secrets,
        }
    }

    #[cfg(target_family = "wasm")]
    pub(crate) fn spawn(connection: ClientConnection) -> Self {
        let secure_for_secrets = connection.secure_for_secrets();
        Self {
            connection,
            secure_for_secrets,
        }
    }

    pub(crate) fn secure_for_secrets(&self) -> bool {
        self.secure_for_secrets
    }

    #[cfg(not(target_family = "wasm"))]
    pub(crate) fn submit(&self, request: RequestEnvelope) -> PendingResponse {
        let request_id = request.request_id;
        let (reply, receiver) = oneshot::channel();
        #[cfg(test)]
        if let Some(connection) = &self.test_connection {
            let _ = reply.send(connection.request(request));
            return PendingResponse {
                request_id,
                reply: receiver,
            };
        }
        if self.jobs.send(Job { request, reply }).is_err() {
            // The worker thread is gone; return a receiver that will
            // immediately resolve to the "stopped before answering" error.
            let (_, receiver) = oneshot::channel();
            return PendingResponse {
                request_id,
                reply: receiver,
            };
        }
        PendingResponse {
            request_id,
            reply: receiver,
        }
    }

    #[cfg(target_family = "wasm")]
    pub(crate) fn submit(&self, request: RequestEnvelope) -> PendingResponse {
        let request_id = request.request_id;
        let (reply, receiver) = oneshot::channel();
        let connection = self.connection.clone();
        wasm_bindgen_futures::spawn_local(async move {
            let response = connection.request(request).await;
            let _ = reply.send(response);
        });
        PendingResponse {
            request_id,
            reply: receiver,
        }
    }
}

#[cfg(all(test, not(target_family = "wasm")))]
mod tests {
    use super::{
        ClientConnection, LoomError, create_session_in_workspace, create_workspace,
        describe_startup_connection_error, include_discovered_models, list_models,
        list_provider_ids, list_workspace_sessions, list_workspaces, negotiate, redact_secret,
        register_workspace, remote_url_is_secure_for_secrets, set_workspace_config,
        unexpected_response, worker_node_status, workspace_config,
    };
    use loom_core::{
        ErrorCode, LoomError as CoreLoomError, Timestamp, WorkspaceId, WorkspaceRecord,
    };
    use loom_model::ModelId;
    use loom_protocol::{ServerResponse, WorkspaceConfig};
    use loom_server::{
        AuthTokenStore, AuthorizationScope, InProcessBackend, RemoteServer, RemoteServerConfig,
    };
    use std::sync::Arc;

    #[test]
    fn provider_secrets_require_tls_or_loopback_transport() {
        assert!(remote_url_is_secure_for_secrets("wss://worker.example/ws"));
        assert!(remote_url_is_secure_for_secrets("ws://localhost:8080/ws"));
        assert!(remote_url_is_secure_for_secrets("ws://127.0.0.1:8080/ws"));
        assert!(remote_url_is_secure_for_secrets("ws://[::1]:8080/ws"));
        assert!(!remote_url_is_secure_for_secrets("ws://worker.example/ws"));
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
            unexpected_response("probe", ServerResponse::SessionRepositoryDetached)
                .message
                .contains("probe")
        );
    }

    #[test]
    fn in_process_connection_exercises_workspace_and_session_client_operations() {
        let connection = ClientConnection::InProcess(Box::new(InProcessBackend::new().connect()));
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
        assert!(!list_models(&connection).unwrap().is_empty());
        assert!(!list_provider_ids(&connection).unwrap().is_empty());
    }

    #[test]
    fn workspace_client_operations_report_backend_errors_and_register_external_records() {
        let connection = ClientConnection::InProcess(Box::new(InProcessBackend::new().connect()));
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
            Err(LoomError::new(
                ErrorCode::ProviderUnavailable,
                "Ollama is unavailable",
                true,
            )),
            &mut models,
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
        let backend = InProcessBackend::new();
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
