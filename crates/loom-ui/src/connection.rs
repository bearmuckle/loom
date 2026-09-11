//! Transport-independent protocol client for the native shell.

use std::sync::{Arc, Mutex};

use std::{
    path::Path,
    sync::mpsc::{self, Receiver, Sender},
    thread,
};

use loom_core::{
    AgentSessionSnapshot, Capability, CapabilitySet, ErrorCode, LoomError, ProjectId, RequestId,
};
use loom_model::{ModelId, ProviderId};
use loom_protocol::{
    AgentRunSnapshot, CURRENT_PROTOCOL_VERSION, ClientRequest, ProjectSnapshot, RequestEnvelope,
    ResponseEnvelope, ServerResponse,
};
use loom_server::{InProcessConnection, WebSocketConnection, WebSocketTransport};

#[derive(Clone)]
pub(crate) enum ClientConnection {
    InProcess(InProcessConnection),
    Remote {
        runtime: Arc<tokio::runtime::Runtime>,
        connection: Arc<Mutex<WebSocketConnection>>,
    },
}

impl ClientConnection {
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

    pub(crate) fn description(&self) -> &'static str {
        match self {
            Self::InProcess(_) => "local",
            Self::Remote { .. } => "remote",
        }
    }
}

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
            Capability::OpenWorkspace,
            Capability::ReadWorkspace,
            Capability::ReadVcsStatus,
            Capability::ReadVcsDiff,
            Capability::ReadTask,
            Capability::StartTask,
            Capability::ControlTask,
            Capability::ReadTaskEvidence,
            Capability::JsonProtocol,
        ]),
    }));
    match response.result? {
        ServerResponse::Negotiated(_) => Ok(()),
        response => Err(unexpected_response("negotiation", response)),
    }
}

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

pub(crate) fn list_projects(
    connection: &ClientConnection,
) -> Result<Vec<ProjectSnapshot>, LoomError> {
    let response = connection.request(RequestEnvelope::new(ClientRequest::ListProjects));
    match response.result? {
        ServerResponse::Projects { projects } => Ok(projects),
        response => Err(unexpected_response("project list", response)),
    }
}

pub(crate) fn select_remote_project(
    projects: &[ProjectSnapshot],
    requested_root: Option<&Path>,
) -> Result<ProjectSnapshot, LoomError> {
    let project = requested_root
        .and_then(|root| {
            let requested = root.to_string_lossy();
            projects.iter().find(|project| {
                project
                    .root
                    .as_deref()
                    .is_some_and(|project_root| project_root == requested)
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

pub(crate) fn list_models(connection: &ClientConnection) -> Result<Vec<ModelId>, LoomError> {
    let response = connection.request(RequestEnvelope::new(ClientRequest::ListModels));
    match response.result? {
        ServerResponse::Models { models } => Ok(models.into_iter().map(|model| model.id).collect()),
        response => Err(unexpected_response("model list", response)),
    }
}

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

/// Owns the protocol connection on a dedicated worker thread.
///
/// UI handlers submit a request and receive a [`PendingResponse`] they await on
/// a background task, so a handler never blocks the GPUI thread on backend
/// latency. One worker executes requests in submission order, which also keeps
/// the remote transport's single in-flight request contract.
#[derive(Clone, Debug)]
pub(crate) struct BackendWorker {
    jobs: Sender<Job>,
}

#[derive(Debug)]
struct Job {
    request: RequestEnvelope,
    reply: Sender<ResponseEnvelope>,
}

/// A submitted request whose response has not arrived yet.
#[derive(Debug)]
pub(crate) struct PendingResponse {
    request_id: RequestId,
    reply: Receiver<ResponseEnvelope>,
}

impl PendingResponse {
    /// Blocks the calling thread until the worker answers.
    ///
    /// This must only be called from a background task, never from a UI
    /// handler.
    pub(crate) fn wait(self) -> ResponseEnvelope {
        self.reply.recv().unwrap_or_else(|_| {
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

impl BackendWorker {
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

    pub(crate) fn submit(&self, request: RequestEnvelope) -> PendingResponse {
        let request_id = request.request_id;
        let (reply, receiver) = mpsc::channel();
        if self.jobs.send(Job { request, reply }).is_err() {
            let (closed, receiver) = mpsc::channel();
            drop(closed);
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
}
