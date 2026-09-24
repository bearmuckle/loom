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
    path::Path,
    sync::mpsc::{self, Sender},
    thread,
};

use futures_channel::oneshot;
use loom_core::{
    AgentSessionSnapshot, Capability, CapabilitySet, ErrorCode, LoomError, ProjectId, RequestId,
};
use loom_model::ModelId;
#[cfg(not(target_family = "wasm"))]
use loom_model::ProviderId;
#[cfg(not(target_family = "wasm"))]
use loom_protocol::AgentRunSnapshot;
use loom_protocol::{
    CURRENT_PROTOCOL_VERSION, ClientRequest, ProjectSnapshot, RequestEnvelope, ResponseEnvelope,
    ServerResponse, WorkerNodeStatus,
};
#[cfg(not(target_family = "wasm"))]
use loom_server::{InProcessConnection, WebSocketConnection, WebSocketTransport};

#[cfg(target_family = "wasm")]
use crate::browser::BrowserConnection;

#[derive(Clone)]
pub(crate) enum ClientConnection {
    #[cfg(not(target_family = "wasm"))]
    InProcess(InProcessConnection),
    #[cfg(not(target_family = "wasm"))]
    Remote {
        runtime: Arc<tokio::runtime::Runtime>,
        connection: Arc<Mutex<WebSocketConnection>>,
    },
    #[cfg(target_family = "wasm")]
    Browser(BrowserConnection),
}

impl ClientConnection {
    #[cfg(not(target_family = "wasm"))]
    pub(crate) fn remote(url: String, token: String) -> Result<Self, LoomError> {
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
        let connection = runtime.block_on(WebSocketTransport::new(&url, &token).connect())?;
        Ok(Self::Remote {
            runtime: Arc::new(runtime),
            connection: Arc::new(Mutex::new(connection)),
        })
    }

    #[cfg(target_family = "wasm")]
    pub(crate) fn browser(url: &str, token: &str) -> Result<Self, LoomError> {
        Ok(Self::Browser(BrowserConnection::connect(url, token)?))
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

    /// Sends a request and awaits its response. Only used in the browser,
    /// where nothing can block the page's single JS thread; the WebSocket
    /// transport resolves this asynchronously as frames arrive.
    #[cfg(target_family = "wasm")]
    pub(crate) async fn request(&self, request: RequestEnvelope) -> ResponseEnvelope {
        match self {
            Self::Browser(connection) => connection.request(request).await,
        }
    }
}

#[cfg(not(target_family = "wasm"))]
pub(crate) fn negotiate(connection: &ClientConnection) -> Result<(), LoomError> {
    let response = connection.request(RequestEnvelope::new(ClientRequest::Negotiate {
        client_version: CURRENT_PROTOCOL_VERSION,
        capabilities: CapabilitySet::new([
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
            Capability::ConfigureApprovalPolicy,
            Capability::OpenWorkspace,
            Capability::ReadWorkspace,
            Capability::ReadVcsStatus,
            Capability::ReadVcsDiff,
            Capability::ReadTask,
            Capability::StartTask,
            Capability::ControlTask,
            Capability::ReadTaskEvidence,
            Capability::ReadWorkerNodeStatus,
            Capability::JsonProtocol,
        ]),
    }));
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
        Capability::ConfigureApprovalPolicy,
        Capability::OpenWorkspace,
        Capability::ReadWorkspace,
        Capability::ReadVcsStatus,
        Capability::ReadVcsDiff,
        Capability::ReadTask,
        Capability::StartTask,
        Capability::ControlTask,
        Capability::ReadTaskEvidence,
        Capability::ReadWorkerNodeStatus,
        Capability::JsonProtocol,
    ])
}

#[cfg(not(target_family = "wasm"))]
pub(crate) fn worker_node_status(
    connection: &ClientConnection,
) -> Result<WorkerNodeStatus, LoomError> {
    let response = connection.request(RequestEnvelope::new(ClientRequest::GetWorkerNodeStatus));
    match response.result? {
        ServerResponse::WorkerNodeStatus(status) => Ok(status),
        response => Err(unexpected_response("worker node status", response)),
    }
}

#[cfg(target_family = "wasm")]
pub(crate) async fn worker_node_status_async(
    connection: &ClientConnection,
) -> Result<WorkerNodeStatus, LoomError> {
    let response = connection
        .request(RequestEnvelope::new(ClientRequest::GetWorkerNodeStatus))
        .await;
    match response.result? {
        ServerResponse::WorkerNodeStatus(status) => Ok(status),
        response => Err(unexpected_response("worker node status", response)),
    }
}

#[cfg(target_family = "wasm")]
pub(crate) async fn negotiate_async(connection: &ClientConnection) -> Result<(), LoomError> {
    let response = connection
        .request(RequestEnvelope::new(ClientRequest::Negotiate {
            client_version: CURRENT_PROTOCOL_VERSION,
            capabilities: negotiation_capabilities(),
        }))
        .await;
    match response.result? {
        ServerResponse::Negotiated(_) => Ok(()),
        response => Err(unexpected_response("negotiation", response)),
    }
}

#[cfg(not(target_family = "wasm"))]
pub(crate) fn open_workspace(
    connection: &ClientConnection,
    project_id: ProjectId,
    root: &Path,
) -> Result<(), LoomError> {
    let response = connection.request(RequestEnvelope::new(ClientRequest::OpenWorkspace {
        project_id,
        root: root.display().to_string(),
    }));
    match response.result? {
        ServerResponse::WorkspaceOpened(_) => Ok(()),
        response => Err(unexpected_response("workspace open", response)),
    }
}

#[cfg(target_family = "wasm")]
pub(crate) async fn open_workspace_async(
    connection: &ClientConnection,
    project_id: ProjectId,
    root: &str,
) -> Result<(), LoomError> {
    let response = connection
        .request(RequestEnvelope::new(ClientRequest::OpenWorkspace {
            project_id,
            root: root.to_owned(),
        }))
        .await;
    match response.result? {
        ServerResponse::WorkspaceOpened(_) => Ok(()),
        response => Err(unexpected_response("workspace open", response)),
    }
}

#[cfg(not(target_family = "wasm"))]
pub(crate) fn create_session(
    connection: &ClientConnection,
    project_id: ProjectId,
    name: &str,
) -> Result<AgentSessionSnapshot, LoomError> {
    let response = connection.request(RequestEnvelope::new(ClientRequest::CreateAgentSession {
        project_id,
        name: name.to_owned(),
    }));
    match response.result? {
        ServerResponse::AgentSessionCreated(snapshot) => Ok(snapshot),
        response => Err(unexpected_response("session creation", response)),
    }
}

#[cfg(target_family = "wasm")]
pub(crate) async fn create_session_async(
    connection: &ClientConnection,
    project_id: ProjectId,
    name: &str,
) -> Result<AgentSessionSnapshot, LoomError> {
    let response = connection
        .request(RequestEnvelope::new(ClientRequest::CreateAgentSession {
            project_id,
            name: name.to_owned(),
        }))
        .await;
    match response.result? {
        ServerResponse::AgentSessionCreated(snapshot) => Ok(snapshot),
        response => Err(unexpected_response("session creation", response)),
    }
}

#[cfg(not(target_family = "wasm"))]
pub(crate) fn list_sessions(
    connection: &ClientConnection,
    project_id: ProjectId,
) -> Result<Vec<AgentSessionSnapshot>, LoomError> {
    let response = connection.request(RequestEnvelope::new(ClientRequest::ListAgentSessions {
        project_id: Some(project_id),
        include_archived: false,
    }));
    match response.result? {
        ServerResponse::AgentSessions { sessions } => Ok(sessions),
        response => Err(unexpected_response("session list", response)),
    }
}

#[cfg(target_family = "wasm")]
pub(crate) async fn list_sessions_async(
    connection: &ClientConnection,
    project_id: ProjectId,
) -> Result<Vec<AgentSessionSnapshot>, LoomError> {
    let response = connection
        .request(RequestEnvelope::new(ClientRequest::ListAgentSessions {
            project_id: Some(project_id),
            include_archived: false,
        }))
        .await;
    match response.result? {
        ServerResponse::AgentSessions { sessions } => Ok(sessions),
        response => Err(unexpected_response("session list", response)),
    }
}

#[cfg(not(target_family = "wasm"))]
pub(crate) fn list_projects(
    connection: &ClientConnection,
) -> Result<Vec<ProjectSnapshot>, LoomError> {
    let response = connection.request(RequestEnvelope::new(ClientRequest::ListProjects));
    match response.result? {
        ServerResponse::Projects { projects } => Ok(projects),
        response => Err(unexpected_response("project list", response)),
    }
}

#[cfg(target_family = "wasm")]
pub(crate) async fn list_projects_async(
    connection: &ClientConnection,
) -> Result<Vec<ProjectSnapshot>, LoomError> {
    let response = connection
        .request(RequestEnvelope::new(ClientRequest::ListProjects))
        .await;
    match response.result? {
        ServerResponse::Projects { projects } => Ok(projects),
        response => Err(unexpected_response("project list", response)),
    }
}

pub(crate) fn select_remote_project(
    projects: &[ProjectSnapshot],
    requested_root: Option<&str>,
) -> Result<ProjectSnapshot, LoomError> {
    let project = requested_root
        .and_then(|root| {
            projects.iter().find(|project| {
                project
                    .root
                    .as_deref()
                    .is_some_and(|project_root| project_root == root)
            })
        })
        .or_else(|| projects.first())
        .cloned()
        .ok_or_else(|| {
            LoomError::new(
                ErrorCode::NotFound,
                "remote backend has no open projects",
                false,
            )
        })?;
    Ok(project)
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
    workspace_root: &Path,
    model: &ModelId,
    task: &str,
) -> Result<AgentRunSnapshot, LoomError> {
    let response = connection.request(RequestEnvelope::new(ClientRequest::StartAgentRun {
        session_id: session.id,
        task: task.to_owned(),
        model: model.clone(),
        workspace_root: workspace_root.display().to_string(),
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
    #[cfg(target_family = "wasm")]
    connection: ClientConnection,
}

#[cfg(not(target_family = "wasm"))]
struct Job {
    request: RequestEnvelope,
    reply: oneshot::Sender<ResponseEnvelope>,
}

impl BackendWorker {
    #[cfg(not(target_family = "wasm"))]
    pub(crate) fn spawn(connection: ClientConnection) -> Self {
        let (jobs, incoming) = mpsc::channel::<Job>();
        thread::spawn(move || {
            while let Ok(job) = incoming.recv() {
                let response = connection.request(job.request);
                // A dropped receiver means the view stopped caring about this
                // request; the backend has already applied it either way.
                let _ = job.reply.send(response);
            }
        });
        Self { jobs }
    }

    #[cfg(target_family = "wasm")]
    pub(crate) fn spawn(connection: ClientConnection) -> Self {
        Self { connection }
    }

    #[cfg(not(target_family = "wasm"))]
    pub(crate) fn submit(&self, request: RequestEnvelope) -> PendingResponse {
        let request_id = request.request_id;
        let (reply, receiver) = oneshot::channel();
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
