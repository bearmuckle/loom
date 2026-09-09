use std::{
    collections::BTreeMap,
    path::PathBuf,
    sync::{Arc, Mutex, MutexGuard},
};

use loom_agent::{
    AgentEvent, AgentRunState, AgentRuntime, AgentRuntimeOptions, AgentRuntimeState, AgentTask,
};
use loom_core::{
    AgentSessionId, AgentSessionState, ApprovalPolicy, Capability, CapabilitySet, ErrorCode,
    EventSequence, LoomError, ProjectId, ProtocolVersion, Result, SessionEventRecord,
};
use loom_model::{ModelDescriptor, ModelId, ProviderId};
use loom_persistence::{CURRENT_SCHEMA_VERSION, FilePersistence};
use loom_process::{TaskSupervisor, TerminalManager};
use loom_protocol::{
    CURRENT_PROTOCOL_VERSION, ClientRequest, NegotiationResult, RequestEnvelope, ResponseEnvelope,
    ServerEventEnvelope, ServerResponse, unsupported_version_error,
};
use loom_providers::{
    CredentialRef, ModelProvider, ProviderConfig, ProviderHealth, ProviderRegistry,
    UnavailableProvider, UsageLedger, deterministic_descriptor,
};
use loom_session::SessionManager;
use loom_tools::ToolExecutor;
use loom_workspace::Workspace;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
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

#[derive(Clone, Debug, Deserialize, Serialize)]
struct PersistedBackendState {
    schema_version: u32,
    sessions: loom_session::SessionManagerState,
    journal: EventJournal,
    runs: BTreeMap<loom_core::RunId, AgentRuntimeState>,
    workspaces: BTreeMap<ProjectId, loom_workspace::WorkspaceStateSnapshot>,
    policies: BTreeMap<ProjectId, ApprovalPolicy>,
    terminal_projects: BTreeMap<loom_core::TerminalId, ProjectId>,
    provider_configs: Vec<ProviderConfig>,
    provider_health: BTreeMap<ProviderId, ProviderHealth>,
    provider_usage: UsageLedger,
    models: Vec<ModelDescriptor>,
}

struct StartRunInput {
    session_id: AgentSessionId,
    task: String,
    model: ModelId,
    workspace_root: String,
    system_instructions: Option<String>,
    repository_instructions: Option<String>,
    options: AgentRuntimeOptions,
}

pub struct InProcessBackend {
    sessions: Mutex<SessionManager>,
    runs: Mutex<BTreeMap<loom_core::RunId, AgentRuntime>>,
    journal: Mutex<EventJournal>,
    workspaces: Mutex<BTreeMap<ProjectId, Workspace>>,
    task_supervisors: Mutex<BTreeMap<ProjectId, TaskSupervisor>>,
    policies: Mutex<BTreeMap<ProjectId, ApprovalPolicy>>,
    terminal_projects: Mutex<BTreeMap<loom_core::TerminalId, ProjectId>>,
    terminals: TerminalManager,
    supported_capabilities: CapabilitySet,
    models: Vec<ModelDescriptor>,
    providers: ProviderRegistry,
    persistence: Option<FilePersistence>,
}

impl InProcessBackend {
    pub fn new() -> Arc<Self> {
        Self::with_provider_registry(ProviderRegistry::demo())
    }

    pub fn with_models(models: Vec<ModelDescriptor>) -> Arc<Self> {
        let credentials = Arc::new(loom_providers::InMemoryCredentialStore::default());
        let providers = ProviderRegistry::with_credentials(credentials);
        for model in &models {
            let existing_provider = providers
                .list_models()
                .unwrap_or_else(|error| panic!("could not inspect provider models: {error}"))
                .iter()
                .any(|existing| existing.provider == model.provider);
            if existing_provider {
                if model.provider.as_str() == "deterministic" {
                    panic!(
                        "deterministic provider only supports model '{}'",
                        deterministic_descriptor().id.as_str()
                    );
                }
                providers
                    .add_model(&model.provider, model.clone())
                    .unwrap_or_else(|error| panic!("could not add provider model: {error}"));
                continue;
            }
            let result = if model.provider.as_str() == "deterministic" {
                providers.register(ProviderConfig::deterministic())
            } else if model.provider.as_str() == "ollama" {
                providers.register(ProviderConfig::ollama(
                    "http://127.0.0.1:11434",
                    model.id.clone(),
                ))
            } else {
                providers.register(ProviderConfig::openai_compatible(
                    model.provider.clone(),
                    "OpenAI-compatible model",
                    "http://127.0.0.1:8000/v1/chat/completions",
                    model.clone(),
                    None,
                ))
            };
            result.unwrap_or_else(|error| panic!("could not register provider model: {error}"));
        }
        Self::with_provider_registry_and_models(providers, models, None)
            .unwrap_or_else(|error| panic!("could not configure providers: {error}"))
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
        let credentials = Arc::new(loom_providers::InMemoryCredentialStore::default());
        credentials.insert(CredentialRef::new("legacy-openai"), api_key.into());
        let providers = ProviderRegistry::with_credentials(credentials);
        let config = ProviderConfig::openai_compatible(
            "openai-compatible",
            "OpenAI-compatible model",
            endpoint,
            descriptor,
            Some(CredentialRef::new("legacy-openai")),
        );
        providers
            .register(config)
            .expect("legacy OpenAI provider configuration is valid");
        Self::with_provider_registry(providers)
    }

    pub fn with_provider_registry(providers: ProviderRegistry) -> Arc<Self> {
        let models = providers
            .list_models()
            .unwrap_or_else(|error| panic!("could not inspect provider models: {error}"));
        Self::with_provider_registry_and_models(providers, models, None)
            .unwrap_or_else(|error| panic!("could not configure providers: {error}"))
    }

    pub fn new_persistent(path: impl Into<PathBuf>) -> Result<Arc<Self>> {
        let providers = ProviderRegistry::demo();
        Self::with_provider_registry_and_models(
            providers,
            Vec::new(),
            Some(FilePersistence::open(path.into())?),
        )
    }

    pub fn open_persistent(path: impl Into<PathBuf>) -> Result<Arc<Self>> {
        Self::new_persistent(path)
    }

    pub fn with_persistence(path: impl Into<PathBuf>) -> Result<Arc<Self>> {
        Self::new_persistent(path)
    }

    pub fn with_provider_registry_persistent(
        providers: ProviderRegistry,
        path: impl Into<PathBuf>,
    ) -> Result<Arc<Self>> {
        Self::with_provider_registry_and_models(
            providers,
            Vec::new(),
            Some(FilePersistence::open(path.into())?),
        )
    }

    fn with_provider_registry_and_models(
        providers: ProviderRegistry,
        models: Vec<ModelDescriptor>,
        persistence: Option<FilePersistence>,
    ) -> Result<Arc<Self>> {
        let models = if models.is_empty() {
            providers.list_models()?
        } else {
            models
        };
        let backend = Arc::new(Self {
            sessions: Mutex::new(SessionManager::default()),
            runs: Mutex::new(BTreeMap::new()),
            journal: Mutex::new(EventJournal::default()),
            workspaces: Mutex::new(BTreeMap::new()),
            task_supervisors: Mutex::new(BTreeMap::new()),
            policies: Mutex::new(BTreeMap::new()),
            terminal_projects: Mutex::new(BTreeMap::new()),
            terminals: TerminalManager::new(),
            supported_capabilities: CapabilitySet::new([
                Capability::CreateAgentSession,
                Capability::ReadAgentSession,
                Capability::SubscribeSessionEvents,
                Capability::StartAgentRun,
                Capability::ReadAgentRun,
                Capability::ControlAgentRun,
                Capability::PauseAgentRun,
                Capability::ResumeAgentRun,
                Capability::ForkAgentSession,
                Capability::RetryFromCheckpoint,
                Capability::ApproveAgentAction,
                Capability::ListProviders,
                Capability::ReadProviderHealth,
                Capability::ReadUsage,
                Capability::InspectContext,
                Capability::OpenWorkspace,
                Capability::ReadWorkspace,
                Capability::WriteWorkspace,
                Capability::SubscribeWorkspaceEvents,
                Capability::OpenTerminal,
                Capability::ControlTerminal,
                Capability::ReadTask,
                Capability::StartTask,
                Capability::ControlTask,
                Capability::ConfigureApprovalPolicy,
                Capability::ManageCheckpoints,
                Capability::TakeoverWorkspace,
                Capability::JsonProtocol,
            ]),
            models,
            providers,
            persistence,
        });
        backend.restore_persisted()?;
        Ok(backend)
    }

    pub fn connect(self: &Arc<Self>) -> InProcessConnection {
        InProcessConnection {
            backend: Arc::clone(self),
            negotiated_capabilities: Arc::new(Mutex::new(None)),
        }
    }

    pub fn provider_registry(&self) -> ProviderRegistry {
        self.providers.clone()
    }

    fn provider(&self, model: &ModelId) -> Result<Box<dyn ModelProvider>> {
        self.providers.create_provider(model)
    }

    fn provider_at(&self, model: &ModelId, cursor: usize) -> Result<Box<dyn ModelProvider>> {
        self.providers.create_provider_at(model, cursor)
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

    fn workspaces(&self) -> Result<MutexGuard<'_, BTreeMap<ProjectId, Workspace>>> {
        self.workspaces.lock().map_err(|_| {
            LoomError::new(
                ErrorCode::Internal,
                "workspace manager lock was poisoned",
                true,
            )
        })
    }

    fn task_supervisors(&self) -> Result<MutexGuard<'_, BTreeMap<ProjectId, TaskSupervisor>>> {
        self.task_supervisors.lock().map_err(|_| {
            LoomError::new(
                ErrorCode::Internal,
                "task supervisor lock was poisoned",
                true,
            )
        })
    }

    fn policies(&self) -> Result<MutexGuard<'_, BTreeMap<ProjectId, ApprovalPolicy>>> {
        self.policies.lock().map_err(|_| {
            LoomError::new(
                ErrorCode::Internal,
                "approval policy lock was poisoned",
                true,
            )
        })
    }

    fn terminal_projects(
        &self,
    ) -> Result<MutexGuard<'_, BTreeMap<loom_core::TerminalId, ProjectId>>> {
        self.terminal_projects.lock().map_err(|_| {
            LoomError::new(
                ErrorCode::Internal,
                "terminal project lock was poisoned",
                true,
            )
        })
    }

    fn restore_persisted(self: &Arc<Self>) -> Result<()> {
        let Some(persistence) = self.persistence.clone() else {
            return Ok(());
        };
        let Some(state) =
            persistence.load_versioned::<PersistedBackendState>(CURRENT_SCHEMA_VERSION)?
        else {
            return Ok(());
        };
        if state.schema_version != CURRENT_SCHEMA_VERSION {
            return Err(LoomError::new(
                ErrorCode::MalformedPayload,
                "persisted backend state has an unsupported schema version",
                false,
            ));
        }
        let sessions = SessionManager::from_state(state.sessions)?;
        {
            let mut target = self.sessions()?;
            *target = sessions;
        }
        {
            let mut target = self.journal()?;
            if state
                .journal
                .events
                .windows(2)
                .any(|events| events[0].sequence >= events[1].sequence)
                || state
                    .journal
                    .events
                    .last()
                    .is_some_and(|event| event.sequence != state.journal.next_sequence)
            {
                return Err(LoomError::new(
                    ErrorCode::MalformedPayload,
                    "persisted event journal sequences are invalid",
                    false,
                ));
            }
            *target = state.journal;
        }
        {
            let mut target = self.policies()?;
            *target = state.policies;
        }
        {
            let mut target = self.terminal_projects()?;
            // Child process handles cannot be restored safely; stale ids must
            // not grant access to a newly created terminal.
            *target = BTreeMap::new();
        }
        self.providers.restore_configs(state.provider_configs)?;
        self.providers.restore_health(state.provider_health)?;
        self.providers.restore_usage(state.provider_usage)?;

        let mut workspaces = self.workspaces()?;
        for (project_id, persisted) in state.workspaces {
            let workspace = Workspace::open(project_id, PathBuf::from(&persisted.root))?;
            workspace.restore_state(persisted)?;
            workspaces.insert(project_id, workspace);
        }
        drop(workspaces);

        let mut restored_runs = BTreeMap::new();
        for (run_id, runtime_state) in state.runs {
            if run_id != runtime_state.run.id {
                return Err(LoomError::new(
                    ErrorCode::MalformedPayload,
                    format!("persisted run key does not match run snapshot {run_id}"),
                    false,
                ));
            }
            let session = self.sessions()?.get(runtime_state.session_id)?;
            let workspace = self
                .workspaces()?
                .get(&session.project_id)
                .cloned()
                .ok_or_else(|| {
                    LoomError::new(
                        ErrorCode::RecoveryRequired,
                        format!("workspace for persisted run {run_id} is unavailable"),
                        true,
                    )
                })?;
            let mut recovery_reason = None;
            let provider = match self
                .provider_at(&runtime_state.task.model, runtime_state.provider_cursor)
            {
                Ok(provider) => provider,
                Err(error) => {
                    let descriptor = self.providers.describe_model(&runtime_state.task.model)?;
                    recovery_reason = Some(error.message.clone());
                    Box::new(UnavailableProvider::new(descriptor, error))
                }
            };
            let tools = ToolExecutor::new_with_workspace(workspace);
            let mut runtime = AgentRuntime::from_state(runtime_state, provider, tools)?;
            if runtime.session_id() != session.id {
                return Err(LoomError::new(
                    ErrorCode::MalformedPayload,
                    format!("persisted run {run_id} references a different session"),
                    false,
                ));
            }
            let mut recovery_events = runtime.recover_after_restart()?;
            if let Some(reason) = recovery_reason {
                recovery_events.push(AgentEvent::RecoveryRequired { run_id, reason });
            }
            restored_runs.insert(run_id, runtime);
            if !recovery_events.is_empty() {
                self.append_recovery_events(session.id, recovery_events)?;
            }
        }
        *self.runs()? = restored_runs;
        self.persist_state()?;
        Ok(())
    }

    fn persist_state(&self) -> Result<()> {
        let Some(persistence) = &self.persistence else {
            return Ok(());
        };
        let runs = self
            .runs()?
            .iter()
            .map(|(run_id, runtime)| (*run_id, runtime.export_state()))
            .collect();
        let workspaces = self
            .workspaces()?
            .iter()
            .map(|(project_id, workspace)| Ok((*project_id, workspace.export_state()?)))
            .collect::<Result<BTreeMap<_, _>>>()?;
        let state = PersistedBackendState {
            schema_version: CURRENT_SCHEMA_VERSION,
            sessions: self.sessions()?.export_state(),
            journal: self.journal()?.clone(),
            runs,
            workspaces,
            policies: self.policies()?.clone(),
            terminal_projects: self.terminal_projects()?.clone(),
            provider_configs: self.providers.export_configs()?,
            provider_health: self.providers.export_health()?,
            provider_usage: self.providers.usage()?,
            models: self.models.clone(),
        };
        persistence.save_versioned(CURRENT_SCHEMA_VERSION, &state)
    }

    pub fn flush(&self) -> Result<()> {
        self.persist_state()
    }

    fn append_recovery_events(
        &self,
        session_id: AgentSessionId,
        events: Vec<AgentEvent>,
    ) -> Result<()> {
        for event in events {
            let state = session_state_for_event(&event);
            self.journal()?.append_agent(session_id, event);
            if let Some(state) = state {
                let current = self.sessions()?.get(session_id)?.state;
                if current != state {
                    let (_, record) = self.sessions()?.transition(session_id, state)?;
                    self.journal()?.append_session(record);
                }
            }
        }
        Ok(())
    }
}

#[derive(Clone)]
pub struct InProcessConnection {
    backend: Arc<InProcessBackend>,
    negotiated_capabilities: Arc<Mutex<Option<CapabilitySet>>>,
}

impl InProcessConnection {
    fn open_workspace(&self, project_id: ProjectId, root: String) -> Result<Workspace> {
        let candidate = Workspace::open(project_id, PathBuf::from(root))?;
        let mut workspaces = self.backend.workspaces()?;
        if let Some(existing) = workspaces.get(&project_id) {
            if existing.root() != candidate.root() {
                return Err(LoomError::conflict(format!(
                    "project {project_id} is already configured for workspace '{}'",
                    existing.root().display()
                )));
            }
            return Ok(existing.clone());
        }
        workspaces.insert(project_id, candidate.clone());
        Ok(candidate)
    }

    fn workspace(&self, project_id: ProjectId) -> Result<Workspace> {
        self.backend
            .workspaces()?
            .get(&project_id)
            .cloned()
            .ok_or_else(|| LoomError::not_found("workspace", project_id))
    }

    fn policy(&self, project_id: ProjectId) -> Result<ApprovalPolicy> {
        let mut policies = self.backend.policies()?;
        Ok(policies
            .entry(project_id)
            .or_insert_with(ApprovalPolicy::default)
            .clone())
    }

    fn task_supervisor(&self, project_id: ProjectId) -> Result<TaskSupervisor> {
        let workspace = self.workspace(project_id)?;
        let mut supervisors = self.backend.task_supervisors()?;
        if let Some(supervisor) = supervisors.get(&project_id) {
            return Ok(supervisor.clone());
        }
        let supervisor = TaskSupervisor::new(workspace.root())?;
        supervisors.insert(project_id, supervisor.clone());
        Ok(supervisor)
    }

    fn check_terminal_project(
        &self,
        project_id: ProjectId,
        terminal_id: loom_core::TerminalId,
    ) -> Result<()> {
        let owner = self
            .backend
            .terminal_projects()?
            .get(&terminal_id)
            .copied()
            .ok_or_else(|| LoomError::not_found("terminal", terminal_id))?;
        if owner != project_id {
            return Err(LoomError::new(
                ErrorCode::WorkspaceAccessDenied,
                "terminal does not belong to the requested workspace",
                false,
            ));
        }
        Ok(())
    }

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
        let result = match result {
            Ok(response) => self.backend.persist_state().map(|()| response),
            Err(error) => Err(error),
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
            ClientRequest::StartAgentRunWithOptions {
                session_id,
                task,
                model,
                workspace_root,
                system_instructions,
                repository_instructions,
                limits,
                context,
            } => self.start_run_with_options(StartRunInput {
                session_id,
                task,
                model,
                workspace_root,
                system_instructions,
                repository_instructions,
                options: AgentRuntimeOptions {
                    limits,
                    context,
                    checkpoint_id: None,
                    ..Default::default()
                },
            }),
            ClientRequest::GetAgentRun { run_id } => {
                let runs = self.backend.runs()?;
                let run = runs
                    .get(&run_id)
                    .ok_or_else(|| LoomError::not_found("agent run", run_id))?;
                Ok(ServerResponse::AgentRun(run.snapshot()))
            }
            ClientRequest::GetRunCheckpoint { run_id } => {
                let (project_id, checkpoint_id) = {
                    let runs = self.backend.runs()?;
                    let runtime = runs
                        .get(&run_id)
                        .ok_or_else(|| LoomError::not_found("agent run", run_id))?;
                    let session = self.backend.sessions()?.get(runtime.session_id())?;
                    (
                        session.project_id,
                        runtime.checkpoint_id().ok_or_else(|| {
                            LoomError::new(
                                ErrorCode::NotFound,
                                format!("agent run {run_id} has no checkpoint"),
                                false,
                            )
                        })?,
                    )
                };
                Ok(ServerResponse::RunCheckpoint(
                    self.workspace(project_id)?.checkpoint(checkpoint_id)?,
                ))
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
            ClientRequest::PauseAgentRun { run_id } => {
                self.control_run(run_id, AgentRuntime::pause)
            }
            ClientRequest::ResumeAgentRun { run_id } => {
                self.control_run(run_id, AgentRuntime::resume)
            }
            ClientRequest::RetryAgentFromCheckpoint {
                run_id,
                checkpoint_id,
            } => self.retry_from_checkpoint(run_id, checkpoint_id),
            ClientRequest::ForkAgentSession { session_id, name } => {
                let (snapshot, record) = self.backend.sessions()?.fork(session_id, name)?;
                let target_id = snapshot.id;
                self.backend.journal()?.append_session(record);
                let history = self
                    .backend
                    .journal()?
                    .events
                    .iter()
                    .filter(|event| {
                        event.session_id == session_id
                            && !matches!(
                                &event.event,
                                loom_protocol::ServerEvent::AgentSessionCreated { .. }
                                    | loom_protocol::ServerEvent::AgentSessionForked { .. }
                            )
                    })
                    .cloned()
                    .collect::<Vec<_>>();
                let mut journal = self.backend.journal()?;
                for event in history {
                    let sequence = journal.next();
                    journal.events.push(ServerEventEnvelope {
                        protocol_version: event.protocol_version,
                        sequence,
                        session_id: target_id,
                        event: event.event,
                    });
                }
                Ok(ServerResponse::AgentSessionForked(snapshot))
            }
            ClientRequest::ListModels => Ok(ServerResponse::Models {
                models: self.backend.providers.list_models()?,
            }),
            ClientRequest::ListProviders => Ok(ServerResponse::Providers {
                providers: self.backend.providers.list_providers()?,
            }),
            ClientRequest::DiscoverProviderModels { provider_id } => Ok(ServerResponse::Models {
                models: self.backend.providers.discover_models(&provider_id)?,
            }),
            ClientRequest::GetProviderHealth { provider_id } => Ok(ServerResponse::ProviderHealth(
                self.backend.providers.check_health(&provider_id)?,
            )),
            ClientRequest::GetRunUsage { run_id } => {
                let runs = self.backend.runs()?;
                let runtime = runs
                    .get(&run_id)
                    .ok_or_else(|| LoomError::not_found("agent run", run_id))?;
                let usage = runtime.usage();
                let provider = self
                    .backend
                    .providers
                    .usage()?
                    .summary(None, Some(&runtime.snapshot().model));
                Ok(ServerResponse::RunUsage { usage, provider })
            }
            ClientRequest::GetSessionUsage { session_id } => {
                self.backend.sessions()?.get(session_id)?;
                let usage = self
                    .backend
                    .runs()?
                    .values()
                    .filter(|runtime| runtime.session_id() == session_id)
                    .fold(loom_core::UsageSnapshot::default(), |mut total, runtime| {
                        let current = runtime.usage();
                        total.add_tokens(
                            current.input_tokens,
                            current.output_tokens,
                            current.cached_input_tokens,
                        );
                        total.tool_calls = total.tool_calls.saturating_add(current.tool_calls);
                        total.cost_micros = total.cost_micros.saturating_add(current.cost_micros);
                        total.elapsed_ms = total.elapsed_ms.max(current.elapsed_ms);
                        total
                    });
                Ok(ServerResponse::SessionUsage {
                    usage,
                    provider: self.backend.providers.usage()?.summary(None, None),
                })
            }
            ClientRequest::InspectAgentContext { run_id } => {
                let runs = self.backend.runs()?;
                let runtime = runs
                    .get(&run_id)
                    .ok_or_else(|| LoomError::not_found("agent run", run_id))?;
                let inspection = runtime.context_inspection().ok_or_else(|| {
                    LoomError::new(
                        ErrorCode::InvalidState,
                        "agent run has not assembled context yet",
                        false,
                    )
                })?;
                Ok(ServerResponse::ContextInspection(inspection))
            }
            ClientRequest::OpenWorkspace { project_id, root } => {
                let workspace = self.open_workspace(project_id, root)?;
                Ok(ServerResponse::WorkspaceOpened(workspace.snapshot()?))
            }
            ClientRequest::GetWorkspaceSnapshot { project_id } => Ok(
                ServerResponse::WorkspaceSnapshot(self.workspace(project_id)?.snapshot()?),
            ),
            ClientRequest::GetWorkspaceEvents {
                project_id,
                after_sequence,
            } => Ok(ServerResponse::WorkspaceEvents {
                events: self.workspace(project_id)?.changes_since(after_sequence)?,
            }),
            ClientRequest::ReadWorkspaceFile { project_id, path } => Ok(
                ServerResponse::WorkspaceFile(self.workspace(project_id)?.read_file(&path)?),
            ),
            ClientRequest::ApplyWorkspaceEdit { project_id, edit } => {
                Ok(ServerResponse::WorkspaceEditApplied(
                    self.workspace(project_id)?.apply_user_edit(edit)?,
                ))
            }
            ClientRequest::TakeWorkspaceControl {
                project_id,
                control,
            } => {
                self.workspace(project_id)?.take_control(control)?;
                Ok(ServerResponse::WorkspaceControl(control))
            }
            ClientRequest::CreateCheckpoint {
                project_id,
                session_id,
                label,
            } => Ok(ServerResponse::CheckpointCreated(
                self.workspace(project_id)?
                    .create_checkpoint(session_id, label)?,
            )),
            ClientRequest::RevertCheckpoint {
                project_id,
                checkpoint_id,
            } => Ok(ServerResponse::CheckpointReverted(
                self.workspace(project_id)?
                    .revert_checkpoint(checkpoint_id)?,
            )),
            ClientRequest::UndoWorkspaceEdit { project_id } => Ok(ServerResponse::WorkspaceUndo(
                self.workspace(project_id)?.undo_last_agent_edit()?,
            )),
            ClientRequest::SetApprovalPolicy { project_id, policy } => {
                self.backend.policies()?.insert(project_id, policy.clone());
                Ok(ServerResponse::ApprovalPolicy(policy))
            }
            ClientRequest::OpenTerminal {
                project_id,
                command,
                args,
                cwd,
            } => {
                let workspace = self.workspace(project_id)?;
                let cwd = workspace.directory_path(cwd.as_deref().unwrap_or("."))?;
                let snapshot = self.backend.terminals.open(command, args, cwd)?;
                self.backend
                    .terminal_projects()?
                    .insert(snapshot.id, project_id);
                Ok(ServerResponse::TerminalOpened(snapshot))
            }
            ClientRequest::WriteTerminalInput {
                project_id,
                terminal_id,
                input,
            } => {
                self.check_terminal_project(project_id, terminal_id)?;
                self.backend.terminals.write_input(terminal_id, &input)?;
                Ok(ServerResponse::Terminal(
                    self.backend.terminals.get(terminal_id)?,
                ))
            }
            ClientRequest::ResizeTerminal {
                project_id,
                terminal_id,
                rows,
                columns,
            } => {
                self.check_terminal_project(project_id, terminal_id)?;
                Ok(ServerResponse::Terminal(self.backend.terminals.resize(
                    terminal_id,
                    rows,
                    columns,
                )?))
            }
            ClientRequest::GetTerminalEvents {
                project_id,
                terminal_id,
                after_sequence,
            } => Ok(ServerResponse::TerminalEvents {
                events: {
                    self.check_terminal_project(project_id, terminal_id)?;
                    self.backend
                        .terminals
                        .events_since(terminal_id, after_sequence)?
                },
            }),
            ClientRequest::CancelTerminal {
                project_id,
                terminal_id,
            } => {
                self.check_terminal_project(project_id, terminal_id)?;
                Ok(ServerResponse::Terminal(
                    self.backend.terminals.cancel(terminal_id)?,
                ))
            }
            ClientRequest::StartTask { project_id, spec } => {
                let supervisor = self.task_supervisor(project_id)?;
                Ok(ServerResponse::TaskStarted(supervisor.start(spec)?))
            }
            ClientRequest::GetTask {
                project_id,
                task_id,
            } => Ok(ServerResponse::Task(
                self.task_supervisor(project_id)?.get(task_id)?,
            )),
            ClientRequest::GetTaskEvents {
                project_id,
                task_id,
                after_sequence,
            } => Ok(ServerResponse::TaskEvents {
                events: self
                    .task_supervisor(project_id)?
                    .events_since(task_id, after_sequence)?,
            }),
            ClientRequest::CancelTask {
                project_id,
                task_id,
            } => Ok(ServerResponse::Task(
                self.task_supervisor(project_id)?.cancel(task_id)?,
            )),
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
        self.start_run_with_options(StartRunInput {
            session_id,
            task,
            model,
            workspace_root,
            system_instructions,
            repository_instructions,
            options: AgentRuntimeOptions::default(),
        })
    }

    fn start_run_with_options(&self, mut input: StartRunInput) -> Result<ServerResponse> {
        let session = self.backend.sessions()?.get(input.session_id)?;
        if session.state != AgentSessionState::Idle {
            return Err(LoomError::new(
                ErrorCode::InvalidState,
                "agent session is already running or completed",
                false,
            ));
        }
        let provider = self.backend.provider(&input.model)?;
        let (input_cost_micros_per_1k, output_cost_micros_per_1k) =
            self.backend.providers.pricing(&input.model)?;
        input.options.input_cost_micros_per_1k = input_cost_micros_per_1k;
        input.options.output_cost_micros_per_1k = output_cost_micros_per_1k;
        let workspace = self.open_workspace(session.project_id, input.workspace_root)?;
        let checkpoint = workspace.create_checkpoint(Some(input.session_id), "before agent run")?;
        input.options.checkpoint_id = Some(checkpoint.id);
        let tools = ToolExecutor::new_with_workspace(workspace);
        let policy = self.policy(session.project_id)?;
        let mut agent_task = AgentTask::new(input.task, input.model)?;
        agent_task.system_instructions = input.system_instructions;
        agent_task.repository_instructions = input.repository_instructions;
        let mut runtime = AgentRuntime::new_with_policy_and_options(
            input.session_id,
            agent_task,
            provider,
            tools,
            policy,
            input.options,
        );
        let run_id = runtime.run_id();
        let events = runtime.start()?;
        let snapshot = runtime.snapshot();
        self.backend.runs()?.insert(run_id, runtime);
        self.record_agent_events(input.session_id, events)?;
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

    fn retry_from_checkpoint(
        &self,
        run_id: loom_core::RunId,
        checkpoint_id: loom_core::CheckpointId,
    ) -> Result<ServerResponse> {
        let (session_id, project_id, persisted_checkpoint) = {
            let runs = self.backend.runs()?;
            let runtime = runs
                .get(&run_id)
                .ok_or_else(|| LoomError::not_found("agent run", run_id))?;
            (
                runtime.session_id(),
                self.backend
                    .sessions()?
                    .get(runtime.session_id())?
                    .project_id,
                runtime.export_state().options.checkpoint_id,
            )
        };
        if persisted_checkpoint != Some(checkpoint_id) {
            return Err(LoomError::conflict(format!(
                "checkpoint {checkpoint_id} is not the checkpoint associated with run {run_id}"
            )));
        }
        self.workspace(project_id)?
            .revert_checkpoint(checkpoint_id)?;
        let (snapshot, events) = {
            let mut runs = self.backend.runs()?;
            let runtime = runs
                .get_mut(&run_id)
                .ok_or_else(|| LoomError::not_found("agent run", run_id))?;
            let events = runtime.retry_from_checkpoint()?;
            (runtime.snapshot(), events)
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
        AgentRunState::Paused => AgentSessionState::Paused,
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
    use std::{fs, path::PathBuf, thread, time::Duration};

    use loom_context::ContextAssemblyOptions;
    use loom_core::{CapabilitySet, PolicyDecision, ProjectId, ToolCallId};
    use loom_process::{TaskEvent, TaskKind, TaskSpec, TaskStatus, TerminalEvent};
    use loom_protocol::{ClientRequest, RequestEnvelope, ServerEvent, ServerResponse};
    use loom_workspace::{WorkspaceControl, WorkspaceEdit};

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

    fn negotiate_m2(connection: &InProcessConnection) {
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
                Capability::OpenWorkspace,
                Capability::ReadWorkspace,
                Capability::WriteWorkspace,
                Capability::SubscribeWorkspaceEvents,
                Capability::OpenTerminal,
                Capability::ControlTerminal,
                Capability::ReadTask,
                Capability::StartTask,
                Capability::ControlTask,
                Capability::ConfigureApprovalPolicy,
                Capability::ManageCheckpoints,
                Capability::TakeoverWorkspace,
            ]),
        }));
        assert!(matches!(response.result, Ok(ServerResponse::Negotiated(_))));
    }

    fn negotiate_m3(connection: &InProcessConnection) {
        let response = connection.request(RequestEnvelope::new(ClientRequest::Negotiate {
            client_version: CURRENT_PROTOCOL_VERSION,
            capabilities: CapabilitySet::new([
                Capability::CreateAgentSession,
                Capability::ReadAgentSession,
                Capability::SubscribeSessionEvents,
                Capability::StartAgentRun,
                Capability::ReadAgentRun,
                Capability::ControlAgentRun,
                Capability::PauseAgentRun,
                Capability::ResumeAgentRun,
                Capability::RetryFromCheckpoint,
                Capability::ForkAgentSession,
                Capability::ApproveAgentAction,
                Capability::ListProviders,
                Capability::ReadProviderHealth,
                Capability::ReadUsage,
                Capability::InspectContext,
                Capability::OpenWorkspace,
                Capability::ReadWorkspace,
                Capability::WriteWorkspace,
                Capability::SubscribeWorkspaceEvents,
                Capability::ManageCheckpoints,
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

    #[test]
    fn exposes_workspace_terminal_task_and_checkpoint_controls() {
        let root = workspace();
        fs::write(root.join("README.md"), "before\n").unwrap();
        let backend = InProcessBackend::new();
        let connection = backend.connect();
        negotiate_m2(&connection);
        let project_id = ProjectId::new();

        let opened = connection.request(RequestEnvelope::new(ClientRequest::OpenWorkspace {
            project_id,
            root: root.display().to_string(),
        }));
        let snapshot = match opened.result.unwrap() {
            ServerResponse::WorkspaceOpened(snapshot) => snapshot,
            response => panic!("unexpected response: {response:?}"),
        };
        assert!(
            snapshot
                .entries
                .iter()
                .any(|entry| entry.path == "README.md")
        );
        let file = connection.request(RequestEnvelope::new(ClientRequest::ReadWorkspaceFile {
            project_id,
            path: "README.md".to_owned(),
        }));
        let revision = match file.result.unwrap() {
            ServerResponse::WorkspaceFile(file) => file.revision,
            response => panic!("unexpected response: {response:?}"),
        };
        let checkpoint =
            connection.request(RequestEnvelope::new(ClientRequest::CreateCheckpoint {
                project_id,
                session_id: None,
                label: "before user edit".to_owned(),
            }));
        let checkpoint_id = match checkpoint.result.unwrap() {
            ServerResponse::CheckpointCreated(checkpoint) => checkpoint.id,
            response => panic!("unexpected response: {response:?}"),
        };
        let edit = connection.request(RequestEnvelope::new(ClientRequest::ApplyWorkspaceEdit {
            project_id,
            edit: WorkspaceEdit {
                path: "README.md".to_owned(),
                old_text: "before".to_owned(),
                new_text: "user".to_owned(),
                expected_revision: Some(revision),
            },
        }));
        assert!(matches!(
            edit.result,
            Ok(ServerResponse::WorkspaceEditApplied(_))
        ));
        let changes = connection.request(RequestEnvelope::new(ClientRequest::GetWorkspaceEvents {
            project_id,
            after_sequence: None,
        }));
        let ServerResponse::WorkspaceEvents { events } = changes.result.unwrap() else {
            panic!("unexpected workspace event response");
        };
        assert!(events.iter().any(|event| event.path == "README.md"));
        let revert = connection.request(RequestEnvelope::new(ClientRequest::RevertCheckpoint {
            project_id,
            checkpoint_id,
        }));
        assert_eq!(revert.result.unwrap_err().code, ErrorCode::Conflict);

        let terminal_command = if cfg!(windows) {
            (
                "cmd".to_owned(),
                vec!["/C".to_owned(), "echo terminal".to_owned()],
            )
        } else {
            ("printf".to_owned(), vec!["terminal".to_owned()])
        };
        let terminal = connection.request(RequestEnvelope::new(ClientRequest::OpenTerminal {
            project_id,
            command: terminal_command.0,
            args: terminal_command.1,
            cwd: None,
        }));
        let terminal_id = match terminal.result.unwrap() {
            ServerResponse::TerminalOpened(snapshot) => snapshot.id,
            response => panic!("unexpected response: {response:?}"),
        };
        let mut terminal_done = false;
        for _ in 0..100 {
            let events =
                connection.request(RequestEnvelope::new(ClientRequest::GetTerminalEvents {
                    project_id,
                    terminal_id,
                    after_sequence: None,
                }));
            let ServerResponse::TerminalEvents { events } = events.result.unwrap() else {
                panic!("unexpected terminal event response");
            };
            if events.iter().any(|event| {
                matches!(
                    event.event,
                    TerminalEvent::Exited {
                        status: loom_process::TerminalStatus::Exited,
                        ..
                    }
                )
            }) {
                terminal_done = true;
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }
        assert!(terminal_done);

        let task_command = if cfg!(windows) {
            (
                "cmd".to_owned(),
                vec!["/C".to_owned(), "echo artifact>artifact.txt".to_owned()],
            )
        } else {
            (
                "sh".to_owned(),
                vec!["-c".to_owned(), "printf artifact > artifact.txt".to_owned()],
            )
        };
        let task = connection.request(RequestEnvelope::new(ClientRequest::StartTask {
            project_id,
            spec: TaskSpec {
                kind: TaskKind::Test,
                label: "M2 task".to_owned(),
                command: task_command.0,
                args: task_command.1,
                cwd: None,
                output_limit_bytes: Some(4096),
                artifact_paths: vec!["artifact.txt".to_owned()],
            },
        }));
        let task_id = match task.result.unwrap() {
            ServerResponse::TaskStarted(snapshot) => snapshot.id,
            response => panic!("unexpected response: {response:?}"),
        };
        let mut task_done = false;
        for _ in 0..100 {
            let current = connection.request(RequestEnvelope::new(ClientRequest::GetTask {
                project_id,
                task_id,
            }));
            let ServerResponse::Task(snapshot) = current.result.unwrap() else {
                panic!("unexpected task response");
            };
            if matches!(
                snapshot.status,
                TaskStatus::Completed | TaskStatus::Failed | TaskStatus::Cancelled
            ) {
                assert!(snapshot.artifacts[0].exists);
                task_done = true;
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }
        assert!(task_done);
        let task_events = connection.request(RequestEnvelope::new(ClientRequest::GetTaskEvents {
            project_id,
            task_id,
            after_sequence: None,
        }));
        let ServerResponse::TaskEvents { events } = task_events.result.unwrap() else {
            panic!("unexpected task event response");
        };
        assert!(
            events
                .iter()
                .any(|event| matches!(event.event, TaskEvent::Completed { .. }))
        );

        let control =
            connection.request(RequestEnvelope::new(ClientRequest::TakeWorkspaceControl {
                project_id,
                control: WorkspaceControl::User,
            }));
        assert!(matches!(
            control.result,
            Ok(ServerResponse::WorkspaceControl(WorkspaceControl::User))
        ));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn policy_decisions_are_visible_and_can_stop_agent_writes() {
        let root = workspace();
        let backend = InProcessBackend::new();
        let connection = backend.connect();
        negotiate_m2(&connection);
        let project_id = ProjectId::new();
        let session = connection.request(RequestEnvelope::new(ClientRequest::CreateAgentSession {
            project_id,
            name: "M2 policy".to_owned(),
        }));
        let session_id = match session.result.unwrap() {
            ServerResponse::AgentSessionCreated(snapshot) => snapshot.id,
            response => panic!("unexpected response: {response:?}"),
        };
        let policy = loom_core::ApprovalPolicy {
            write: PolicyDecision::Deny,
            ..Default::default()
        };
        let policy_response =
            connection.request(RequestEnvelope::new(ClientRequest::SetApprovalPolicy {
                project_id,
                policy,
            }));
        assert!(matches!(
            policy_response.result,
            Ok(ServerResponse::ApprovalPolicy(_))
        ));
        let started = connection.request(RequestEnvelope::new(ClientRequest::StartAgentRun {
            session_id,
            task: "attempt a write".to_owned(),
            model: ModelId::new("deterministic/demo"),
            workspace_root: root.display().to_string(),
            system_instructions: None,
            repository_instructions: None,
        }));
        let run_id = match started.result.unwrap() {
            ServerResponse::AgentRunStarted(snapshot) => snapshot.id,
            response => panic!("unexpected response: {response:?}"),
        };
        let events = connection.request(RequestEnvelope::new(ClientRequest::GetSessionEvents {
            session_id: Some(session_id),
            after_sequence: None,
        }));
        let ServerResponse::SessionEvents { events } = events.result.unwrap() else {
            panic!("unexpected session event response");
        };
        assert!(events.iter().any(|event| {
            matches!(
                &event.event,
                ServerEvent::Agent {
                    event: loom_agent::AgentEvent::ToolPolicyEvaluated {
                        run_id: event_run,
                        evaluation,
                        ..
                    }
                } if *event_run == run_id && evaluation.decision == PolicyDecision::Deny
            )
        }));
        let run = connection.request(RequestEnvelope::new(ClientRequest::GetAgentRun { run_id }));
        let ServerResponse::AgentRun(snapshot) = run.result.unwrap() else {
            panic!("unexpected run response");
        };
        assert_eq!(snapshot.state, AgentRunState::Failed);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn persistent_backend_recovers_transcript_workspace_and_pending_approval() {
        let root = workspace();
        let persistence =
            std::env::temp_dir().join(format!("loom-server-state-{}.json", ProjectId::new()));
        let project_id = ProjectId::new();
        let (session_id, run_id, approval_id) = {
            let backend = InProcessBackend::new_persistent(&persistence).unwrap();
            let connection = backend.connect();
            negotiate_m3(&connection);
            let session =
                connection.request(RequestEnvelope::new(ClientRequest::CreateAgentSession {
                    project_id,
                    name: "durable run".to_owned(),
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
            let events = match connection
                .request(RequestEnvelope::new(ClientRequest::GetSessionEvents {
                    session_id: Some(session_id),
                    after_sequence: None,
                }))
                .result
                .unwrap()
            {
                ServerResponse::SessionEvents { events } => events,
                response => panic!("unexpected response: {response:?}"),
            };
            let approval_id = events
                .iter()
                .find_map(|event| match &event.event {
                    ServerEvent::Agent {
                        event: AgentEvent::ToolApprovalRequired { call, .. },
                    } => Some(call.id),
                    _ => None,
                })
                .unwrap();
            (session_id, run_id, approval_id)
        };
        assert!(persistence.is_file());

        let backend = InProcessBackend::new_persistent(&persistence).unwrap();
        let connection = backend.connect();
        negotiate_m3(&connection);
        let recovered =
            connection.request(RequestEnvelope::new(ClientRequest::GetAgentRun { run_id }));
        let ServerResponse::AgentRun(snapshot) = recovered.result.unwrap() else {
            panic!("unexpected run response");
        };
        assert_eq!(snapshot.state, AgentRunState::AwaitingApproval);
        let events = connection.request(RequestEnvelope::new(ClientRequest::GetSessionEvents {
            session_id: Some(session_id),
            after_sequence: None,
        }));
        let ServerResponse::SessionEvents { events } = events.result.unwrap() else {
            panic!("unexpected events response");
        };
        assert!(events.len() >= 5);
        let checkpoint =
            connection.request(RequestEnvelope::new(ClientRequest::GetRunCheckpoint {
                run_id,
            }));
        let ServerResponse::RunCheckpoint(checkpoint) = checkpoint.result.unwrap() else {
            panic!("unexpected checkpoint response");
        };
        assert_eq!(checkpoint.session_id, Some(session_id));

        let response =
            connection.request(RequestEnvelope::new(ClientRequest::ApproveAgentAction {
                run_id,
                tool_call_id: approval_id,
            }));
        assert!(response.result.is_ok());
        let events = match connection
            .request(RequestEnvelope::new(ClientRequest::GetSessionEvents {
                session_id: Some(session_id),
                after_sequence: None,
            }))
            .result
            .unwrap()
        {
            ServerResponse::SessionEvents { events } => events,
            response => panic!("unexpected events response: {response:?}"),
        };
        let command_approval = events
            .iter()
            .find_map(|event| match &event.event {
                ServerEvent::Agent {
                    event: AgentEvent::ToolApprovalRequired { call, .. },
                } if call.name == "run_command" => Some(call.id),
                _ => None,
            })
            .unwrap();
        let response =
            connection.request(RequestEnvelope::new(ClientRequest::ApproveAgentAction {
                run_id,
                tool_call_id: command_approval,
            }));
        assert!(response.result.is_ok());
        let usage = connection.request(RequestEnvelope::new(ClientRequest::GetRunUsage { run_id }));
        let ServerResponse::RunUsage { usage, .. } = usage.result.unwrap() else {
            panic!("unexpected usage response");
        };
        assert_eq!(usage.input_tokens, 240);
        assert_eq!(usage.output_tokens, 52);
        assert_eq!(usage.tool_calls, 3);
        let workspace_snapshot =
            connection.request(RequestEnvelope::new(ClientRequest::GetWorkspaceSnapshot {
                project_id,
            }));
        assert!(workspace_snapshot.result.is_ok());
        drop(connection);
        drop(backend);
        let reopened = InProcessBackend::new_persistent(&persistence).unwrap();
        let reopened_connection = reopened.connect();
        negotiate_m3(&reopened_connection);
        let recovered_usage = reopened_connection
            .request(RequestEnvelope::new(ClientRequest::GetRunUsage { run_id }));
        let ServerResponse::RunUsage { usage, .. } = recovered_usage.result.unwrap() else {
            panic!("unexpected recovered usage response");
        };
        assert_eq!(usage.input_tokens, 240);
        assert_eq!(usage.output_tokens, 52);
        let retried = reopened_connection.request(RequestEnvelope::new(
            ClientRequest::RetryAgentFromCheckpoint {
                run_id,
                checkpoint_id: checkpoint.id,
            },
        ));
        let ServerResponse::AgentRun(snapshot) = retried.result.unwrap() else {
            panic!("unexpected retry response");
        };
        assert_eq!(snapshot.state, AgentRunState::AwaitingApproval);
        fs::remove_file(persistence).unwrap();
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn pause_resume_fork_and_provider_discovery_are_protocol_operations() {
        let root = workspace();
        let backend = InProcessBackend::new();
        let connection = backend.connect();
        negotiate_m3(&connection);
        let project_id = ProjectId::new();
        let session = connection.request(RequestEnvelope::new(ClientRequest::CreateAgentSession {
            project_id,
            name: "control run".to_owned(),
        }));
        let session_id = match session.result.unwrap() {
            ServerResponse::AgentSessionCreated(snapshot) => snapshot.id,
            response => panic!("unexpected response: {response:?}"),
        };
        let started = connection.request(RequestEnvelope::new(ClientRequest::StartAgentRun {
            session_id,
            task: "control".to_owned(),
            model: ModelId::new("deterministic/demo"),
            workspace_root: root.display().to_string(),
            system_instructions: None,
            repository_instructions: None,
        }));
        let run_id = match started.result.unwrap() {
            ServerResponse::AgentRunStarted(snapshot) => snapshot.id,
            response => panic!("unexpected response: {response:?}"),
        };
        let paused = connection.request(RequestEnvelope::new(ClientRequest::PauseAgentRun {
            run_id,
        }));
        let ServerResponse::AgentRun(snapshot) = paused.result.unwrap() else {
            panic!("unexpected pause response");
        };
        assert_eq!(snapshot.state, AgentRunState::Paused);
        let resumed = connection.request(RequestEnvelope::new(ClientRequest::ResumeAgentRun {
            run_id,
        }));
        let ServerResponse::AgentRun(snapshot) = resumed.result.unwrap() else {
            panic!("unexpected resume response");
        };
        assert_eq!(snapshot.state, AgentRunState::AwaitingApproval);

        let forked = connection.request(RequestEnvelope::new(ClientRequest::ForkAgentSession {
            session_id,
            name: "control fork".to_owned(),
        }));
        let forked_id = match forked.result.unwrap() {
            ServerResponse::AgentSessionForked(snapshot) => snapshot.id,
            response => panic!("unexpected fork response: {response:?}"),
        };
        assert_ne!(forked_id, session_id);

        let providers = connection.request(RequestEnvelope::new(ClientRequest::ListProviders));
        let ServerResponse::Providers { providers } = providers.result.unwrap() else {
            panic!("unexpected provider response");
        };
        assert!(
            providers
                .iter()
                .any(|provider| provider.kind == loom_providers::ProviderKind::Ollama)
        );
        assert!(
            providers
                .iter()
                .any(|provider| provider.kind == loom_providers::ProviderKind::Deterministic)
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn explicit_limits_and_context_inspection_are_durable_protocol_state() {
        let root = workspace();
        let backend = InProcessBackend::new();
        let connection = backend.connect();
        negotiate_m3(&connection);
        let project_id = ProjectId::new();
        let session = connection.request(RequestEnvelope::new(ClientRequest::CreateAgentSession {
            project_id,
            name: "limited run".to_owned(),
        }));
        let session_id = match session.result.unwrap() {
            ServerResponse::AgentSessionCreated(snapshot) => snapshot.id,
            response => panic!("unexpected response: {response:?}"),
        };
        let started = connection.request(RequestEnvelope::new(
            ClientRequest::StartAgentRunWithOptions {
                session_id,
                task: "limited".to_owned(),
                model: ModelId::new("deterministic/demo"),
                workspace_root: root.display().to_string(),
                system_instructions: Some("system".to_owned()),
                repository_instructions: Some("repository".to_owned()),
                limits: loom_core::SessionLimits {
                    max_tool_calls: Some(0),
                    ..Default::default()
                },
                context: ContextAssemblyOptions {
                    context_window: Some(1_024),
                    max_input_tokens: Some(512),
                    reserved_output_tokens: Some(128),
                },
            },
        ));
        let run_id = match started.result.unwrap() {
            ServerResponse::AgentRunStarted(snapshot) => {
                assert_eq!(snapshot.state, AgentRunState::Failed);
                snapshot.id
            }
            response => panic!("unexpected response: {response:?}"),
        };
        let events = match connection
            .request(RequestEnvelope::new(ClientRequest::GetSessionEvents {
                session_id: Some(session_id),
                after_sequence: None,
            }))
            .result
            .unwrap()
        {
            ServerResponse::SessionEvents { events } => events,
            response => panic!("unexpected response: {response:?}"),
        };
        assert!(events.iter().any(|event| {
            matches!(
                &event.event,
                ServerEvent::Agent {
                    event: AgentEvent::RunLimitReached { status, .. }
                } if status.exceeded.contains(&loom_core::LimitKind::ToolCalls)
            )
        }));
        let usage = connection.request(RequestEnvelope::new(ClientRequest::GetSessionUsage {
            session_id,
        }));
        let ServerResponse::SessionUsage { usage, .. } = usage.result.unwrap() else {
            panic!("unexpected session usage response");
        };
        assert_eq!(usage.tool_calls, 0);
        let context =
            connection.request(RequestEnvelope::new(ClientRequest::InspectAgentContext {
                run_id,
            }));
        assert_eq!(context.result.unwrap_err().code, ErrorCode::InvalidState);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn malformed_persisted_backend_state_is_rejected_without_fallback() {
        let path =
            std::env::temp_dir().join(format!("loom-server-malformed-{}.json", ProjectId::new()));
        fs::write(&path, br#"{"schema_version":1,"state":{"broken":true}}"#).unwrap();
        let error = match InProcessBackend::new_persistent(&path) {
            Ok(_) => panic!("malformed backend state unexpectedly loaded"),
            Err(error) => error,
        };
        assert_eq!(error.code, ErrorCode::MalformedPayload);
        fs::remove_file(path).unwrap();
    }

    #[allow(dead_code)]
    fn _keep_tool_id_in_scope(_: ToolCallId) {}
}
