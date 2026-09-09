use std::{
    collections::BTreeMap,
    path::PathBuf,
    sync::{Arc, Mutex, MutexGuard},
};

use loom_agent::{AgentEvent, AgentRunState, AgentRuntime, AgentTask};
use loom_core::{
    AgentSessionId, AgentSessionState, Capability, CapabilitySet, ErrorCode, EventSequence,
    LoomError, ProtocolVersion, Result, SessionEventRecord,
};
use loom_model::{ModelDescriptor, ModelId};
use loom_protocol::{
    CURRENT_PROTOCOL_VERSION, ClientRequest, NegotiationResult, RequestEnvelope, ResponseEnvelope,
    ServerEventEnvelope, ServerResponse, unsupported_version_error,
};
use loom_providers::{
    DeterministicProvider, ModelProvider, OpenAiCompatibleProvider, deterministic_descriptor,
};
use loom_session::SessionManager;
use loom_tools::ToolExecutor;

enum ProviderConfiguration {
    Deterministic,
    OpenAiCompatible { endpoint: String, api_key: String },
}

#[derive(Default)]
struct EventJournal {
    next_sequence: EventSequence,
    events: Vec<ServerEventEnvelope>,
}

impl EventJournal {
    fn append_session(&mut self, record: SessionEventRecord) {
        let sequence = self.next();
        self.events.push(ServerEventEnvelope::from_session_event(
            sequence,
            record.session_id,
            record.event,
        ));
    }

    fn append_agent(&mut self, session_id: AgentSessionId, event: AgentEvent) {
        let sequence = self.next();
        self.events.push(ServerEventEnvelope::from_agent_event(
            sequence, session_id, event,
        ));
    }

    fn next(&mut self) -> EventSequence {
        self.next_sequence = self.next_sequence.next();
        self.next_sequence
    }
}

pub struct InProcessBackend {
    sessions: Mutex<SessionManager>,
    runs: Mutex<BTreeMap<loom_core::RunId, AgentRuntime>>,
    journal: Mutex<EventJournal>,
    supported_capabilities: CapabilitySet,
    models: Vec<ModelDescriptor>,
    provider_configuration: ProviderConfiguration,
}

impl InProcessBackend {
    pub fn new() -> Arc<Self> {
        Self::with_models(vec![deterministic_descriptor()])
    }

    pub fn with_models(models: Vec<ModelDescriptor>) -> Arc<Self> {
        Self::configured(models, ProviderConfiguration::Deterministic)
    }

    pub fn with_openai_compatible(
        endpoint: impl Into<String>,
        api_key: impl Into<String>,
        model: impl Into<ModelId>,
    ) -> Arc<Self> {
        let model = model.into();
        let descriptor = ModelDescriptor {
            id: model,
            provider: loom_model::ProviderId::new("openai-compatible"),
            display_name: "OpenAI-compatible model".to_owned(),
            context_window: None,
            capabilities: loom_model::ModelCapabilities {
                streaming: false,
                tool_calling: true,
                vision: false,
                json_mode: true,
            },
        };
        Self::configured(
            vec![descriptor],
            ProviderConfiguration::OpenAiCompatible {
                endpoint: endpoint.into(),
                api_key: api_key.into(),
            },
        )
    }

    fn configured(
        models: Vec<ModelDescriptor>,
        provider_configuration: ProviderConfiguration,
    ) -> Arc<Self> {
        Arc::new(Self {
            sessions: Mutex::new(SessionManager::default()),
            runs: Mutex::new(BTreeMap::new()),
            journal: Mutex::new(EventJournal::default()),
            supported_capabilities: CapabilitySet::new([
                Capability::CreateAgentSession,
                Capability::ReadAgentSession,
                Capability::SubscribeSessionEvents,
                Capability::StartAgentRun,
                Capability::ReadAgentRun,
                Capability::ControlAgentRun,
                Capability::ApproveAgentAction,
                Capability::JsonProtocol,
            ]),
            models,
            provider_configuration,
        })
    }

    pub fn connect(self: &Arc<Self>) -> InProcessConnection {
        InProcessConnection {
            backend: Arc::clone(self),
            negotiated_capabilities: Arc::new(Mutex::new(None)),
        }
    }

    fn provider(&self, model: &ModelId) -> Result<Box<dyn ModelProvider>> {
        match &self.provider_configuration {
            ProviderConfiguration::Deterministic if model.as_str() == "deterministic/demo" => {
                Ok(Box::new(DeterministicProvider::demo()))
            }
            ProviderConfiguration::OpenAiCompatible { endpoint, api_key } => Ok(Box::new(
                OpenAiCompatibleProvider::new(endpoint, api_key, model.clone()),
            )),
            ProviderConfiguration::Deterministic => Err(LoomError::new(
                ErrorCode::ProviderUnavailable,
                format!("model '{}' is not available", model.as_str()),
                false,
            )),
        }
    }

    fn sessions(&self) -> Result<MutexGuard<'_, SessionManager>> {
        self.sessions.lock().map_err(|_| {
            LoomError::new(
                ErrorCode::Internal,
                "session manager lock was poisoned",
                true,
            )
        })
    }

    fn runs(&self) -> Result<MutexGuard<'_, BTreeMap<loom_core::RunId, AgentRuntime>>> {
        self.runs
            .lock()
            .map_err(|_| LoomError::new(ErrorCode::Internal, "agent run lock was poisoned", true))
    }

    fn journal(&self) -> Result<MutexGuard<'_, EventJournal>> {
        self.journal.lock().map_err(|_| {
            LoomError::new(ErrorCode::Internal, "event journal lock was poisoned", true)
        })
    }
}

#[derive(Clone)]
pub struct InProcessConnection {
    backend: Arc<InProcessBackend>,
    negotiated_capabilities: Arc<Mutex<Option<CapabilitySet>>>,
}

impl InProcessConnection {
    pub fn request(&self, request: RequestEnvelope) -> ResponseEnvelope {
        let request_id = request.request_id;
        if !request
            .protocol_version
            .is_compatible_with(CURRENT_PROTOCOL_VERSION)
        {
            return ResponseEnvelope::failure(
                request_id,
                unsupported_version_error(request.protocol_version),
            );
        }

        let result = match request.request {
            ClientRequest::Negotiate {
                client_version,
                capabilities,
            } => self.negotiate(client_version, capabilities),
            request => self.handle_after_negotiation(request),
        };

        match result {
            Ok(response) => ResponseEnvelope::success(request_id, response),
            Err(error) => ResponseEnvelope::failure(request_id, error),
        }
    }

    fn negotiate(
        &self,
        client_version: ProtocolVersion,
        capabilities: CapabilitySet,
    ) -> Result<ServerResponse> {
        if !client_version.is_compatible_with(CURRENT_PROTOCOL_VERSION) {
            return Err(unsupported_version_error(client_version));
        }
        let negotiated = capabilities.intersection(&self.backend.supported_capabilities);
        *self.negotiated_capabilities()? = Some(negotiated.clone());

        Ok(ServerResponse::Negotiated(NegotiationResult {
            protocol_version: CURRENT_PROTOCOL_VERSION,
            capabilities: negotiated,
        }))
    }

    fn handle_after_negotiation(&self, request: ClientRequest) -> Result<ServerResponse> {
        let capabilities = self
            .negotiated_capabilities()?
            .clone()
            .ok_or_else(|| LoomError::invalid_request("connection must negotiate first"))?;
        if let Some(required) = request.required_capability() {
            if !capabilities.contains(required) {
                return Err(LoomError::new(
                    ErrorCode::CapabilityDenied,
                    format!("connection did not negotiate capability {required:?}"),
                    false,
                ));
            }
        }

        match request {
            ClientRequest::Negotiate { .. } => unreachable!("negotiation is handled above"),
            ClientRequest::CreateAgentSession { project_id, name } => {
                let (snapshot, record) = self.backend.sessions()?.create(project_id, name)?;
                self.backend.journal()?.append_session(record);
                Ok(ServerResponse::AgentSessionCreated(snapshot))
            }
            ClientRequest::GetAgentSession { session_id } => {
                let snapshot = self.backend.sessions()?.get(session_id)?;
                Ok(ServerResponse::AgentSession(snapshot))
            }
            ClientRequest::GetSessionEvents {
                session_id,
                after_sequence,
            } => {
                let events = self
                    .backend
                    .journal()?
                    .events
                    .iter()
                    .filter(|event| {
                        session_id.is_none_or(|id| event.session_id == id)
                            && after_sequence.is_none_or(|sequence| event.sequence > sequence)
                    })
                    .cloned()
                    .collect();
                Ok(ServerResponse::SessionEvents { events })
            }
            ClientRequest::StartAgentRun {
                session_id,
                task,
                model,
                workspace_root,
                system_instructions,
                repository_instructions,
            } => self.start_run(
                session_id,
                task,
                model,
                workspace_root,
                system_instructions,
                repository_instructions,
            ),
            ClientRequest::GetAgentRun { run_id } => {
                let runs = self.backend.runs()?;
                let run = runs
                    .get(&run_id)
                    .ok_or_else(|| LoomError::not_found("agent run", run_id))?;
                Ok(ServerResponse::AgentRun(run.snapshot()))
            }
            ClientRequest::ApproveAgentAction {
                run_id,
                tool_call_id,
            } => self.control_run(run_id, |run| run.approve(tool_call_id)),
            ClientRequest::RejectAgentAction {
                run_id,
                tool_call_id,
                reason,
            } => self.control_run(run_id, |run| run.reject(tool_call_id, reason)),
            ClientRequest::InterruptAgentRun { run_id } => {
                self.control_run(run_id, AgentRuntime::interrupt)
            }
            ClientRequest::RetryAgentStep { run_id } => {
                self.control_run(run_id, AgentRuntime::retry)
            }
            ClientRequest::ListModels => Ok(ServerResponse::Models {
                models: self.backend.models.clone(),
            }),
        }
    }

    fn start_run(
        &self,
        session_id: AgentSessionId,
        task: String,
        model: ModelId,
        workspace_root: String,
        system_instructions: Option<String>,
        repository_instructions: Option<String>,
    ) -> Result<ServerResponse> {
        let session = self.backend.sessions()?.get(session_id)?;
        if session.state != AgentSessionState::Idle {
            return Err(LoomError::new(
                ErrorCode::InvalidState,
                "agent session is already running or completed",
                false,
            ));
        }
        let provider = self.backend.provider(&model)?;
        let tools = ToolExecutor::new(PathBuf::from(workspace_root))?;
        let mut agent_task = AgentTask::new(task, model)?;
        agent_task.system_instructions = system_instructions;
        agent_task.repository_instructions = repository_instructions;
        let mut runtime = AgentRuntime::new(session_id, agent_task, provider, tools);
        let run_id = runtime.run_id();
        let events = runtime.start()?;
        let snapshot = runtime.snapshot();
        self.backend.runs()?.insert(run_id, runtime);
        self.record_agent_events(session_id, events)?;
        Ok(ServerResponse::AgentRunStarted(snapshot))
    }

    fn control_run(
        &self,
        run_id: loom_core::RunId,
        operation: impl FnOnce(&mut AgentRuntime) -> Result<Vec<AgentEvent>>,
    ) -> Result<ServerResponse> {
        let (session_id, snapshot, events) = {
            let mut runs = self.backend.runs()?;
            let runtime = runs
                .get_mut(&run_id)
                .ok_or_else(|| LoomError::not_found("agent run", run_id))?;
            let session_id = runtime.session_id();
            let events = operation(runtime)?;
            let snapshot = runtime.snapshot();
            (session_id, snapshot, events)
        };
        self.record_agent_events(session_id, events)?;
        Ok(ServerResponse::AgentRun(snapshot))
    }

    fn record_agent_events(
        &self,
        session_id: AgentSessionId,
        events: Vec<AgentEvent>,
    ) -> Result<()> {
        for event in events {
            let state = session_state_for_event(&event);
            self.backend.journal()?.append_agent(session_id, event);
            if let Some(state) = state {
                self.sync_session_state(session_id, state)?;
            }
        }
        Ok(())
    }

    fn sync_session_state(
        &self,
        session_id: AgentSessionId,
        state: AgentSessionState,
    ) -> Result<()> {
        let current = self.backend.sessions()?.get(session_id)?.state;
        if current == state {
            return Ok(());
        }
        let (_, record) = self.backend.sessions()?.transition(session_id, state)?;
        self.backend.journal()?.append_session(record);
        Ok(())
    }

    fn negotiated_capabilities(&self) -> Result<MutexGuard<'_, Option<CapabilitySet>>> {
        self.negotiated_capabilities.lock().map_err(|_| {
            LoomError::new(
                ErrorCode::Internal,
                "connection state lock was poisoned",
                true,
            )
        })
    }
}

fn session_state_for_event(event: &AgentEvent) -> Option<AgentSessionState> {
    let state = match event {
        AgentEvent::RunStarted { snapshot } => snapshot.state,
        AgentEvent::RunStateChanged { state, .. } => *state,
        AgentEvent::RunCompleted { snapshot } => snapshot.state,
        _ => return None,
    };
    Some(match state {
        AgentRunState::Planning => AgentSessionState::Planning,
        AgentRunState::Executing => AgentSessionState::Executing,
        AgentRunState::AwaitingApproval => AgentSessionState::AwaitingApproval,
        AgentRunState::Evaluating => AgentSessionState::Evaluating,
        AgentRunState::Completed => AgentSessionState::Completed,
        AgentRunState::Failed => AgentSessionState::Failed,
        AgentRunState::Cancelled => AgentSessionState::Cancelled,
    })
}

impl InProcessConnection {
    pub fn disconnected(backend: Arc<InProcessBackend>) -> Self {
        Self {
            backend,
            negotiated_capabilities: Arc::new(Mutex::new(None)),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{fs, path::PathBuf};

    use loom_core::{CapabilitySet, ProjectId, ToolCallId};
    use loom_protocol::{ClientRequest, RequestEnvelope, ServerEvent, ServerResponse};

    use super::*;

    fn negotiate(connection: &InProcessConnection) {
        let response = connection.request(RequestEnvelope::new(ClientRequest::Negotiate {
            client_version: CURRENT_PROTOCOL_VERSION,
            capabilities: CapabilitySet::new([
                Capability::CreateAgentSession,
                Capability::ReadAgentSession,
                Capability::SubscribeSessionEvents,
                Capability::StartAgentRun,
                Capability::ReadAgentRun,
                Capability::ControlAgentRun,
                Capability::ApproveAgentAction,
            ]),
        }));
        assert!(matches!(response.result, Ok(ServerResponse::Negotiated(_))));
    }

    fn workspace() -> PathBuf {
        let root = std::env::temp_dir().join(format!("loom-server-{}", ProjectId::new()));
        fs::create_dir_all(&root).unwrap();
        root
    }

    #[test]
    fn creates_session_and_reads_event_stream() {
        let backend = InProcessBackend::new();
        let connection = backend.connect();
        negotiate(&connection);

        let create = connection.request(RequestEnvelope::new(ClientRequest::CreateAgentSession {
            project_id: ProjectId::new(),
            name: "In-process demo".to_owned(),
        }));
        let session_id = match create.result.unwrap() {
            ServerResponse::AgentSessionCreated(snapshot) => snapshot.id,
            response => panic!("unexpected response: {response:?}"),
        };

        let events = connection.request(RequestEnvelope::new(ClientRequest::GetSessionEvents {
            session_id: Some(session_id),
            after_sequence: None,
        }));
        let ServerResponse::SessionEvents { events } = events.result.unwrap() else {
            panic!("unexpected response");
        };
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].session_id, session_id);
    }

    #[test]
    fn runs_deterministic_agent_through_approvals() {
        let root = workspace();
        let backend = InProcessBackend::new();
        let connection = backend.connect();
        negotiate(&connection);
        let session = connection.request(RequestEnvelope::new(ClientRequest::CreateAgentSession {
            project_id: ProjectId::new(),
            name: "M1 run".to_owned(),
        }));
        let session_id = match session.result.unwrap() {
            ServerResponse::AgentSessionCreated(snapshot) => snapshot.id,
            response => panic!("unexpected response: {response:?}"),
        };
        let started = connection.request(RequestEnvelope::new(ClientRequest::StartAgentRun {
            session_id,
            task: "create a demo file".to_owned(),
            model: ModelId::new("deterministic/demo"),
            workspace_root: root.display().to_string(),
            system_instructions: Some("Be concise.".to_owned()),
            repository_instructions: Some("Keep changes focused.".to_owned()),
        }));
        let run_id = match started.result.unwrap() {
            ServerResponse::AgentRunStarted(snapshot) => snapshot.id,
            response => panic!("unexpected response: {response:?}"),
        };

        let mut after = None;
        loop {
            let response =
                connection.request(RequestEnvelope::new(ClientRequest::GetSessionEvents {
                    session_id: Some(session_id),
                    after_sequence: after,
                }));
            let ServerResponse::SessionEvents { events } = response.result.unwrap() else {
                panic!("unexpected response");
            };
            let mut completed = false;
            for event in &events {
                after = Some(event.sequence);
                if let ServerEvent::Agent {
                    event:
                        AgentEvent::ToolApprovalRequired {
                            run_id: event_run,
                            call,
                        },
                } = &event.event
                {
                    assert_eq!(*event_run, run_id);
                    let response = connection.request(RequestEnvelope::new(
                        ClientRequest::ApproveAgentAction {
                            run_id,
                            tool_call_id: call.id,
                        },
                    ));
                    assert!(response.result.is_ok());
                }
                if matches!(
                    &event.event,
                    ServerEvent::Agent {
                        event: AgentEvent::RunCompleted { .. }
                    }
                ) {
                    completed = true;
                }
            }
            if completed {
                break;
            }
        }
        let final_run =
            connection.request(RequestEnvelope::new(ClientRequest::GetAgentRun { run_id }));
        let ServerResponse::AgentRun(snapshot) = final_run.result.unwrap() else {
            panic!("unexpected response");
        };
        assert_eq!(snapshot.state, AgentRunState::Completed);
        assert!(root.join("loom-m1-demo.txt").is_file());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn requires_negotiation_before_session_requests() {
        let backend = InProcessBackend::new();
        let connection = backend.connect();
        let response =
            connection.request(RequestEnvelope::new(ClientRequest::CreateAgentSession {
                project_id: ProjectId::new(),
                name: "Rejected".to_owned(),
            }));

        assert_eq!(response.result.unwrap_err().code, ErrorCode::InvalidRequest);
    }

    #[test]
    fn unknown_run_is_structured_not_found() {
        let backend = InProcessBackend::new();
        let connection = backend.connect();
        negotiate(&connection);

        let response = connection.request(RequestEnvelope::new(ClientRequest::GetAgentRun {
            run_id: loom_core::RunId::new(),
        }));

        assert_eq!(response.result.unwrap_err().code, ErrorCode::NotFound);
    }

    #[allow(dead_code)]
    fn _keep_tool_id_in_scope(_: ToolCallId) {}
}
