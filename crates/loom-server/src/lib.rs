use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
    sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError, Weak},
    time::{Duration, Instant},
};

use loom_agent::{
    AgentEvent, AgentEventObserver, AgentRunSnapshot, AgentRunState, AgentRuntime,
    AgentRuntimeOptions, AgentRuntimeState, AgentTask, RunControl, RunProgress,
};
use loom_core::{
    AgentSessionId, AgentSessionState, ApprovalPolicy, Capability, CapabilitySet, ErrorCode,
    EventSequence, LoomError, ProjectId, ProtocolVersion, Result, SessionEventRecord, Timestamp,
};
use loom_model::{ModelCapabilities, ModelDescriptor, ModelId, ProviderId};
use loom_persistence::{CURRENT_SCHEMA_VERSION, FilePersistence};
use loom_process::{TaskSupervisor, TerminalManager};
use loom_protocol::{
    AgentRunSnapshotProjection, AgentSessionSnapshotProjection, CURRENT_PROTOCOL_VERSION,
    ClientRequest, NegotiationResult, ProjectSnapshot, RequestEnvelope, ResponseEnvelope,
    ServerEventEnvelope, ServerResponse, WorkerNodeResources, WorkerNodeStatus, WorkspaceConfig,
    unsupported_version_error,
};
use loom_providers::{
    CredentialRef, FileCredentialStore, GITHUB_COPILOT_CREDENTIAL_REF, ModelProvider,
    ProviderConfig, ProviderHealth, ProviderRegistry, UnavailableProvider, UsageLedger,
    deterministic_descriptor,
};
use loom_session::SessionManager;
use loom_tools::ToolExecutor;
use loom_vcs::GitService;
use loom_workspace::Workspace;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::Value;
use sysinfo::System;

mod auth;
mod remote;

fn json_value<T: Serialize>(value: T) -> Result<Value> {
    serde_json::to_value(value).map_err(|error| {
        LoomError::new(
            ErrorCode::Persistence,
            format!("could not serialize persistence section: {error}"),
            false,
        )
    })
}

fn from_json<T: DeserializeOwned>(value: Value) -> Result<T> {
    serde_json::from_value(value).map_err(|error| {
        LoomError::new(
            ErrorCode::MalformedPayload,
            format!("persisted state contains malformed JSON: {error}"),
            false,
        )
    })
}

fn worker_node_url_is_safe(url: &str) -> bool {
    let Ok(url) = url::Url::parse(url) else {
        return false;
    };
    if !matches!(url.scheme(), "ws" | "wss")
        || url.host_str().is_none_or(str::is_empty)
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
    {
        return false;
    }
    !url.query_pairs().any(|(key, _)| {
        matches!(
            key.to_ascii_lowercase().as_str(),
            "token"
                | "access_token"
                | "auth"
                | "authorization"
                | "password"
                | "api_key"
                | "secret"
                | "credential"
                | "bearer"
        )
    })
}

pub use auth::{AuthSession, AuthTokenStore, AuthorizationScope, IssuedToken};
pub use remote::{
    RemoteServer, RemoteServerConfig, RunningRemoteServer, WebSocketConnection, WebSocketTransport,
};

const DEFAULT_EVENT_RETENTION: usize = 4096;
const IDEMPOTENCY_RETENTION: usize = 1024;
const MAX_REVIEW_CHANGES: usize = 512;
const MAX_REVIEW_DIFF_BYTES: usize = 64 * 1024;
const MAX_REVIEW_FILE_BYTES: usize = 128 * 1024;
const MAX_RUN_MESSAGE_BYTES: usize = 32 * 1024;

fn github_copilot_credentials() -> Result<Arc<FileCredentialStore>> {
    let credentials = Arc::new(FileCredentialStore::open(
        FileCredentialStore::default_path(),
    )?);
    if let Ok(token) = std::env::var("LOOM_GITHUB_TOKEN")
        && !token.trim().is_empty()
    {
        credentials.insert(CredentialRef::new(GITHUB_COPILOT_CREDENTIAL_REF), token)?;
    }
    Ok(credentials)
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
struct EventJournal {
    next_sequence: EventSequence,
    events: Vec<ServerEventEnvelope>,
    #[serde(default = "default_event_retention")]
    retention_limit: usize,
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
        if self.retention_limit == 0 {
            self.retention_limit = DEFAULT_EVENT_RETENTION;
        }
        let excess = self
            .events
            .len()
            .saturating_add(1)
            .saturating_sub(self.retention_limit);
        if excess > 0 {
            self.events.drain(..excess);
        }
        self.next_sequence
    }

    fn is_cursor_stale(&self, after_sequence: Option<EventSequence>) -> bool {
        let Some(after_sequence) = after_sequence else {
            return false;
        };
        self.events
            .first()
            .is_some_and(|first| after_sequence.next() < first.sequence)
    }

    fn set_retention(&mut self, limit: usize) {
        self.retention_limit = limit;
        let excess = self.events.len().saturating_sub(limit);
        if excess > 0 {
            self.events.drain(..excess);
        }
    }

    fn events_since(
        &self,
        session_id: Option<AgentSessionId>,
        after_sequence: Option<EventSequence>,
    ) -> Vec<ServerEventEnvelope> {
        self.events
            .iter()
            .filter(|event| {
                session_id.is_none_or(|id| event.session_id == id)
                    && after_sequence.is_none_or(|sequence| event.sequence > sequence)
            })
            .cloned()
            .collect()
    }

    fn recent_events(&self, session_id: AgentSessionId, limit: usize) -> Vec<ServerEventEnvelope> {
        self.events
            .iter()
            .filter(|event| event.session_id == session_id)
            .rev()
            .take(limit)
            .cloned()
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect()
    }
}

fn default_event_retention() -> usize {
    DEFAULT_EVENT_RETENTION
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct IdempotencyRecord {
    request: ClientRequest,
    response: ResponseEnvelope,
}

#[derive(Clone, Debug)]
struct PersistedBackendState {
    sessions: loom_session::SessionManagerState,
    journal: EventJournal,
    runs: BTreeMap<loom_core::RunId, AgentRuntimeState>,
    workspaces: BTreeMap<ProjectId, loom_workspace::WorkspaceStateSnapshot>,
    policies: BTreeMap<ProjectId, ApprovalPolicy>,
    provider_configs: Vec<ProviderConfig>,
    provider_health: BTreeMap<ProviderId, ProviderHealth>,
    workspace_configs: BTreeMap<ProjectId, WorkspaceConfig>,
    provider_usage: UsageLedger,
    idempotency: BTreeMap<loom_core::RequestId, IdempotencyRecord>,
}

/// One agent run owned by the backend.
///
/// The runtime lock is held only while a step is executing. Reads and control
/// requests use the cached state and the control handle instead, so a running
/// model call never blocks another request.
struct RunHandle {
    run_id: loom_core::RunId,
    session_id: AgentSessionId,
    runtime: Mutex<AgentRuntime>,
    control: RunControl,
    state: Mutex<AgentRuntimeState>,
    running: Mutex<bool>,
    idle: Condvar,
    failure: Mutex<Option<LoomError>>,
}

/// Whether a control request pauses a run or ends it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RunStop {
    Interrupt,
    Pause,
}

/// How long a control request waits for a running step to honour it.
const CONTROL_SETTLE_TIMEOUT: Duration = Duration::from_secs(30);
/// How long an operation that needs exclusive runtime access waits for a step
/// that is already finishing. A run that is genuinely busy is reported as a
/// retryable conflict instead.
const ENTRY_SETTLE_TIMEOUT: Duration = Duration::from_secs(2);

impl RunHandle {
    fn new(runtime: AgentRuntime) -> Self {
        Self {
            run_id: runtime.run_id(),
            session_id: runtime.session_id(),
            control: runtime.control(),
            state: Mutex::new(runtime.export_state()),
            runtime: Mutex::new(runtime),
            running: Mutex::new(false),
            idle: Condvar::new(),
            failure: Mutex::new(None),
        }
    }

    fn state(&self) -> AgentRuntimeState {
        self.locked_state().clone()
    }

    fn snapshot(&self) -> AgentRunSnapshot {
        self.locked_state().run.clone()
    }

    fn locked_state(&self) -> MutexGuard<'_, AgentRuntimeState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn refresh(&self, runtime: &AgentRuntime) {
        *self.locked_state() = runtime.export_state();
    }

    /// Keeps the cached run state current while a step is still executing.
    ///
    /// The transcript in the cached state is only replaced when the step ends;
    /// live message deltas are observable through the event journal.
    fn apply_event(&self, event: &AgentEvent) {
        let mut state = self.locked_state();
        match event {
            AgentEvent::RunStarted { snapshot } | AgentEvent::RunCompleted { snapshot } => {
                state.run = snapshot.clone();
            }
            AgentEvent::RunStateChanged {
                state: run_state, ..
            } => {
                state.run.state = *run_state;
                state.run.updated_at = Timestamp::now();
            }
            AgentEvent::RunUsageUpdated { usage, .. } => state.usage = usage.clone(),
            AgentEvent::ToolApprovalRequired { call, .. } => {
                state.pending_approval = Some(call.clone());
            }
            AgentEvent::ToolApprovalDecided { .. } => state.pending_approval = None,
            AgentEvent::NeedsInput { prompt, .. } => {
                state.pending_input = Some(prompt.clone());
            }
            AgentEvent::UserMessage { .. } => state.pending_input = None,
            AgentEvent::ActivityRecorded { activity, .. } => {
                if let Some(existing) = state
                    .activities
                    .iter_mut()
                    .find(|existing| existing.id == activity.id)
                {
                    *existing = activity.clone();
                } else {
                    state.activities.push(activity.clone());
                }
            }
            _ => {}
        }
    }

    fn is_running(&self) -> bool {
        *self.running.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn set_running(&self, running: bool) {
        *self.running.lock().unwrap_or_else(PoisonError::into_inner) = running;
        self.idle.notify_all();
    }

    /// Locks the runtime for an operation that requires exclusive access.
    ///
    /// Returns a retryable conflict instead of blocking when a step is in
    /// flight, so a caller is never parked behind a model call.
    fn try_runtime(&self) -> Result<MutexGuard<'_, AgentRuntime>> {
        match self.runtime.try_lock() {
            Ok(runtime) => Ok(runtime),
            Err(std::sync::TryLockError::Poisoned(poisoned)) => Ok(poisoned.into_inner()),
            Err(std::sync::TryLockError::WouldBlock) => Err(LoomError::new(
                ErrorCode::Conflict,
                format!("agent run {} is executing a step", self.run_id),
                true,
            )),
        }
    }

    /// Locks the runtime for an operation that responds to a state the run has
    /// already reached, allowing the worker a moment to finish its last step.
    fn runtime_for_entry(&self) -> Result<MutexGuard<'_, AgentRuntime>> {
        // Approval events are journaled before the worker releases the runtime
        // lock. Give that transition the same settle time as other control
        // operations so a client can approve as soon as the prompt appears.
        let settle_timeout = if self.state().run.state == AgentRunState::AwaitingApproval {
            CONTROL_SETTLE_TIMEOUT
        } else {
            ENTRY_SETTLE_TIMEOUT
        };
        if self.is_running() && self.wait_until_idle_for(settle_timeout).is_err() {
            return Err(LoomError::new(
                ErrorCode::Conflict,
                format!("agent run {} is executing a step", self.run_id),
                true,
            ));
        }
        self.try_runtime()
    }

    /// Waits for an in-flight step to observe a pause or interrupt request.
    fn wait_until_idle(&self) -> Result<()> {
        self.wait_until_idle_for(CONTROL_SETTLE_TIMEOUT)
    }

    fn wait_until_idle_for(&self, timeout: Duration) -> Result<()> {
        let deadline = Instant::now() + timeout;
        let mut running = self.running.lock().unwrap_or_else(PoisonError::into_inner);
        while *running {
            let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
                return Err(LoomError::new(
                    ErrorCode::DeadlineExceeded,
                    format!("agent run {} did not stop in time", self.run_id),
                    true,
                ));
            };
            let (guard, timeout) = self
                .idle
                .wait_timeout(running, remaining)
                .unwrap_or_else(PoisonError::into_inner);
            running = guard;
            if timeout.timed_out() && *running {
                return Err(LoomError::new(
                    ErrorCode::DeadlineExceeded,
                    format!("agent run {} did not stop in time", self.run_id),
                    true,
                ));
            }
        }
        Ok(())
    }

    fn take_failure(&self) -> Option<LoomError> {
        self.failure
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
    }

    fn record_failure(&self, error: LoomError) {
        let mut failure = self.failure.lock().unwrap_or_else(PoisonError::into_inner);
        if failure.is_none() {
            *failure = Some(error);
        }
    }
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
    node_id: String,
    node_name: String,
    sessions: Mutex<SessionManager>,
    runs: Mutex<BTreeMap<loom_core::RunId, Arc<RunHandle>>>,
    journal: Mutex<EventJournal>,
    workspaces: Mutex<BTreeMap<ProjectId, Workspace>>,
    vcs: Mutex<BTreeMap<ProjectId, GitService>>,
    task_supervisors: Mutex<BTreeMap<ProjectId, TaskSupervisor>>,
    policies: Mutex<BTreeMap<ProjectId, ApprovalPolicy>>,
    workspace_configs: Mutex<BTreeMap<ProjectId, WorkspaceConfig>>,
    terminal_projects: Mutex<BTreeMap<loom_core::TerminalId, ProjectId>>,
    terminals: TerminalManager,
    resource_monitor: Mutex<ResourceMonitor>,
    supported_capabilities: CapabilitySet,
    models: Vec<ModelDescriptor>,
    providers: ProviderRegistry,
    persistence: Option<FilePersistence>,
    idempotency: Mutex<BTreeMap<loom_core::RequestId, IdempotencyRecord>>,
    in_flight_requests: Mutex<BTreeMap<loom_core::RequestId, Arc<Mutex<()>>>>,
    session_admissions: Mutex<BTreeMap<AgentSessionId, Arc<Mutex<()>>>>,
    self_reference: Mutex<Weak<InProcessBackend>>,
}

#[derive(Default)]
struct ResourceMonitor {
    system: System,
    has_cpu_baseline: bool,
}

impl ResourceMonitor {
    fn sample(
        &mut self,
        disk_total_bytes: Option<u64>,
        disk_available_bytes: Option<u64>,
    ) -> WorkerNodeResources {
        self.system.refresh_cpu_all();
        self.system.refresh_memory();

        let cpu_usage_percent = if self.has_cpu_baseline {
            cpu_usage_percent(self.system.global_cpu_usage())
        } else {
            None
        };
        self.has_cpu_baseline = true;

        let memory_total = self.system.total_memory();
        let memory_available = self.system.available_memory();
        let memory_total_bytes = (memory_total > 0).then_some(memory_total);
        let memory_available_bytes = memory_total_bytes.map(|_| memory_available.min(memory_total));

        WorkerNodeResources {
            cpu_count: std::thread::available_parallelism()
                .map(|count| count.get())
                .unwrap_or(1),
            cpu_usage_percent,
            memory_usage_percent: memory_usage_percent(memory_total_bytes, memory_available_bytes),
            memory_total_bytes,
            memory_available_bytes,
            disk_total_bytes,
            disk_available_bytes,
        }
    }
}

fn cpu_usage_percent(usage: f32) -> Option<u8> {
    usage
        .is_finite()
        .then(|| usage.clamp(0.0, 100.0).round() as u8)
}

fn memory_usage_percent(total: Option<u64>, available: Option<u64>) -> Option<u8> {
    let (Some(total), Some(available)) = (total.filter(|total| *total > 0), available) else {
        return None;
    };
    let usage = total.saturating_sub(available.min(total)) as f64 / total as f64 * 100.0;
    Some(usage.round() as u8)
}

fn worker_node_identity() -> (String, String) {
    let node_id = uuid::Uuid::new_v4().to_string();
    let hostname = ["HOSTNAME", "COMPUTERNAME"]
        .iter()
        .find_map(|key| {
            std::env::var(key)
                .ok()
                .filter(|value| !value.trim().is_empty())
        })
        .unwrap_or_else(|| "Loom backend".to_owned());
    let node_name = format!("{hostname} · {}", &node_id[..8]);
    (node_id, node_name)
}

impl InProcessBackend {
    pub fn new() -> Arc<Self> {
        Self::with_provider_registry(ProviderRegistry::demo())
    }

    pub fn new_with_github_copilot() -> Result<Arc<Self>> {
        let credentials = github_copilot_credentials()?;
        Self::with_provider_registry_persistent_credentials(
            ProviderRegistry::configured(credentials)?,
            None,
        )
    }

    pub fn demo_with_github_copilot() -> Result<Arc<Self>> {
        let credentials = github_copilot_credentials()?;
        Self::with_provider_registry_persistent_credentials(
            ProviderRegistry::demo_with_credentials(credentials),
            None,
        )
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

    pub fn with_openai_compatible_persistent(
        endpoint: impl Into<String>,
        api_key: impl Into<String>,
        model: impl Into<ModelId>,
        path: impl Into<PathBuf>,
    ) -> Result<Arc<Self>> {
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
        let api_key = api_key.into();
        let credential = if api_key.is_empty() {
            None
        } else {
            let reference = CredentialRef::new("ui-openai-compatible");
            credentials.insert(reference.clone(), api_key);
            Some(reference)
        };
        let providers = ProviderRegistry::with_credentials(credentials);
        providers.register(ProviderConfig::openai_compatible(
            "openai-compatible",
            "OpenAI-compatible model",
            endpoint,
            descriptor,
            credential,
        ))?;
        Self::with_provider_registry_persistent(providers, path)
    }

    pub fn with_openai_compatible_persistent_with_github_copilot(
        endpoint: impl Into<String>,
        api_key: impl Into<String>,
        model: impl Into<ModelId>,
        path: impl Into<PathBuf>,
    ) -> Result<Arc<Self>> {
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
        let credentials = github_copilot_credentials()?;
        credentials.insert(CredentialRef::new("ui-openai-compatible"), api_key.into())?;
        let providers = ProviderRegistry::with_credentials(credentials);
        providers.register(ProviderConfig::openai_compatible(
            "openai-compatible",
            "OpenAI-compatible model",
            endpoint,
            descriptor,
            Some(CredentialRef::new("ui-openai-compatible")),
        ))?;
        providers.register(ProviderConfig::github_copilot(CredentialRef::new(
            GITHUB_COPILOT_CREDENTIAL_REF,
        )))?;
        Self::with_provider_registry_persistent(providers, path)
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

    pub fn new_persistent_with_github_copilot(path: impl Into<PathBuf>) -> Result<Arc<Self>> {
        let credentials = github_copilot_credentials()?;
        Self::with_provider_registry_persistent_credentials(
            ProviderRegistry::configured(credentials)?,
            Some(path.into()),
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

    fn with_provider_registry_persistent_credentials(
        providers: ProviderRegistry,
        path: Option<PathBuf>,
    ) -> Result<Arc<Self>> {
        Self::with_provider_registry_and_models(
            providers,
            Vec::new(),
            path.map(FilePersistence::open).transpose()?,
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
        let (node_id, node_name) = worker_node_identity();
        let backend = Arc::new(Self {
            node_id,
            node_name,
            sessions: Mutex::new(SessionManager::default()),
            runs: Mutex::new(BTreeMap::new()),
            journal: Mutex::new(EventJournal::default()),
            workspaces: Mutex::new(BTreeMap::new()),
            vcs: Mutex::new(BTreeMap::new()),
            task_supervisors: Mutex::new(BTreeMap::new()),
            policies: Mutex::new(BTreeMap::new()),
            workspace_configs: Mutex::new(BTreeMap::new()),
            terminal_projects: Mutex::new(BTreeMap::new()),
            terminals: TerminalManager::new(),
            resource_monitor: Mutex::new(ResourceMonitor::default()),
            supported_capabilities: CapabilitySet::new([
                Capability::CreateAgentSession,
                Capability::ReadAgentSession,
                Capability::ControlAgentSession,
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
                Capability::ReadWorkspaceInstructions,
                Capability::ReadVcsStatus,
                Capability::ReadVcsDiff,
                Capability::ReadTaskEvidence,
                Capability::ReadWorkerNodeStatus,
                Capability::JsonProtocol,
            ]),
            models,
            providers,
            persistence,
            idempotency: Mutex::new(BTreeMap::new()),
            in_flight_requests: Mutex::new(BTreeMap::new()),
            session_admissions: Mutex::new(BTreeMap::new()),
            self_reference: Mutex::new(Weak::new()),
        });
        *backend
            .self_reference
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Arc::downgrade(&backend);
        backend.restore_persisted()?;
        Ok(backend)
    }

    pub fn connect(self: &Arc<Self>) -> InProcessConnection {
        InProcessConnection {
            backend: Arc::clone(self),
            negotiated_capabilities: Arc::new(Mutex::new(None)),
            auth: None,
        }
    }

    pub fn connect_authenticated(self: &Arc<Self>, auth: AuthSession) -> InProcessConnection {
        InProcessConnection {
            backend: Arc::clone(self),
            negotiated_capabilities: Arc::new(Mutex::new(None)),
            auth: Some(auth),
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

    fn runs(&self) -> Result<MutexGuard<'_, BTreeMap<loom_core::RunId, Arc<RunHandle>>>> {
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

    fn vcs(&self) -> Result<MutexGuard<'_, BTreeMap<ProjectId, GitService>>> {
        self.vcs.lock().map_err(|_| {
            LoomError::new(
                ErrorCode::Internal,
                "Git service manager lock was poisoned",
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

    fn workspace_configs(&self) -> Result<MutexGuard<'_, BTreeMap<ProjectId, WorkspaceConfig>>> {
        self.workspace_configs.lock().map_err(|_| {
            LoomError::new(
                ErrorCode::Internal,
                "workspace config lock was poisoned",
                true,
            )
        })
    }

    fn resource_monitor(&self) -> Result<MutexGuard<'_, ResourceMonitor>> {
        self.resource_monitor.lock().map_err(|_| {
            LoomError::new(
                ErrorCode::Internal,
                "worker resource monitor lock was poisoned",
                true,
            )
        })
    }

    fn set_workspace_config(&self, project_id: ProjectId, config: WorkspaceConfig) -> Result<()> {
        let unique_urls = config
            .worker_nodes
            .iter()
            .map(|node| node.url.as_str())
            .collect::<BTreeSet<_>>();
        if config.worker_nodes.len() > 64
            || unique_urls.len() != config.worker_nodes.len()
            || config
                .worker_nodes
                .iter()
                .any(|node| node.url.trim() != node.url || !worker_node_url_is_safe(&node.url))
        {
            return Err(LoomError::invalid_request(
                "workspace worker-node configuration must contain at most 64 WebSocket URLs without access tokens",
            ));
        }
        let current_revision = self
            .workspace_configs()?
            .get(&project_id)
            .map(|config| config.revision);
        if current_revision.is_some_and(|revision| revision > config.revision) {
            return Ok(());
        }
        let previous = self.workspace_configs()?.insert(project_id, config);
        if let Err(error) = self.persist_state() {
            let mut configs = self.workspace_configs()?;
            if let Some(previous) = previous {
                configs.insert(project_id, previous);
            } else {
                configs.remove(&project_id);
            }
            return Err(error);
        }
        Ok(())
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
        let mut needs_persist = false;
        let Some(sessions) = persistence.load_section::<loom_session::SessionManagerState>(
            "sessions",
            CURRENT_SCHEMA_VERSION,
        )?
        else {
            return Ok(());
        };
        let required = |name: &str| -> Result<Value> {
            persistence
                .load_section(name, CURRENT_SCHEMA_VERSION)?
                .ok_or_else(|| {
                    LoomError::new(
                        ErrorCode::MalformedPayload,
                        format!("persistence section '{name}' is missing"),
                        false,
                    )
                })
        };
        let state = PersistedBackendState {
            sessions,
            journal: from_json(required("journal")?)?,
            runs: from_json(required("runs")?)?,
            workspaces: from_json(required("workspaces")?)?,
            policies: from_json(required("policies")?)?,
            provider_configs: from_json(required("provider_configs")?)?,
            provider_health: from_json(required("provider_health")?)?,
            workspace_configs: persistence
                .load_section("workspace_configs", CURRENT_SCHEMA_VERSION)?
                .map(from_json)
                .transpose()?
                .unwrap_or_default(),
            provider_usage: from_json(required("provider_usage")?)?,
            idempotency: from_json(required("idempotency")?)?,
        };
        let _: BTreeMap<loom_core::TerminalId, ProjectId> =
            from_json(required("terminal_projects")?)?;
        let _: Vec<ModelDescriptor> = from_json(required("models")?)?;
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
            let mut target = self.idempotency()?;
            *target = state.idempotency;
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
        *self.workspace_configs()? = state.workspace_configs;

        let mut workspaces = self.workspaces()?;
        for (project_id, persisted) in state.workspaces {
            let original = persisted.clone();
            let workspace = Workspace::open(project_id, PathBuf::from(&persisted.root))?;
            workspace.restore_state(persisted)?;
            needs_persist |= workspace.export_state()? != original;
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
            let provider =
                match self.provider_at(&runtime_state.task.model, runtime_state.provider_cursor) {
                    Ok(provider) => provider,
                    Err(error) => {
                        let descriptor = self
                            .providers
                            .describe_model(&runtime_state.task.model)
                            .unwrap_or_else(|_| ModelDescriptor {
                                id: runtime_state.task.model.clone(),
                                provider: ProviderId::new("recovered"),
                                display_name: "Unavailable persisted model".to_owned(),
                                context_window: None,
                                capabilities: ModelCapabilities::default(),
                            });
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
            restored_runs.insert(run_id, self.register_runtime(runtime));
            if !recovery_events.is_empty() {
                self.append_recovery_events(session.id, recovery_events)?;
                needs_persist = true;
            }
        }
        *self.runs()? = restored_runs;
        if needs_persist {
            self.persist_state()?;
        }
        Ok(())
    }

    fn persist_state(&self) -> Result<()> {
        let Some(persistence) = &self.persistence else {
            return Ok(());
        };
        let runs: BTreeMap<loom_core::RunId, AgentRuntimeState> = self
            .runs()?
            .iter()
            .map(|(run_id, handle)| (*run_id, handle.state()))
            .collect();
        let workspaces = self
            .workspaces()?
            .iter()
            .map(|(project_id, workspace)| Ok((*project_id, workspace.export_state()?)))
            .collect::<Result<BTreeMap<_, _>>>()?;
        persistence.save_sections(
            CURRENT_SCHEMA_VERSION,
            &[
                ("sessions", json_value(self.sessions()?.export_state())?),
                ("journal", json_value(self.journal()?.clone())?),
                ("runs", json_value(runs)?),
                ("workspaces", json_value(workspaces)?),
                ("policies", json_value(self.policies()?.clone())?),
                (
                    "workspace_configs",
                    json_value(self.workspace_configs()?.clone())?,
                ),
                (
                    "terminal_projects",
                    json_value(self.terminal_projects()?.clone())?,
                ),
                (
                    "provider_configs",
                    json_value(self.providers.export_configs()?)?,
                ),
                (
                    "provider_health",
                    json_value(self.providers.export_health()?)?,
                ),
                ("provider_usage", json_value(self.providers.usage()?)?),
                ("models", json_value(self.models.clone())?),
                ("idempotency", json_value(self.idempotency()?.clone())?),
            ],
        )
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

    fn idempotency(
        &self,
    ) -> Result<MutexGuard<'_, BTreeMap<loom_core::RequestId, IdempotencyRecord>>> {
        self.idempotency.lock().map_err(|_| {
            LoomError::new(
                ErrorCode::Internal,
                "idempotency cache lock was poisoned",
                true,
            )
        })
    }

    /// Serializes retries of one request id without serializing unrelated
    /// mutations, so a long-running request cannot block a control request.
    fn request_slot(&self, request_id: loom_core::RequestId) -> Result<Arc<Mutex<()>>> {
        let mut in_flight = self.in_flight_requests.lock().map_err(|_| {
            LoomError::new(
                ErrorCode::Internal,
                "request serialization lock was poisoned",
                true,
            )
        })?;
        Ok(Arc::clone(in_flight.entry(request_id).or_default()))
    }

    fn session_admission(&self, session_id: AgentSessionId) -> Result<Arc<Mutex<()>>> {
        let mut admissions = self.session_admissions.lock().map_err(|_| {
            LoomError::new(
                ErrorCode::Internal,
                "session admission lock was poisoned",
                true,
            )
        })?;
        Ok(Arc::clone(admissions.entry(session_id).or_default()))
    }

    fn release_request_slot(&self, request_id: loom_core::RequestId) {
        let mut in_flight = self
            .in_flight_requests
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if in_flight
            .get(&request_id)
            .is_some_and(|slot| Arc::strong_count(slot) == 1)
        {
            in_flight.remove(&request_id);
        }
    }

    /// Journals one agent event and keeps the session state in step with it.
    fn record_agent_event(&self, session_id: AgentSessionId, event: AgentEvent) -> Result<()> {
        let state = session_state_for_event(&event);
        self.journal()?.append_agent(session_id, event);
        if let Some(state) = state {
            let current = self.sessions()?.get(session_id)?.state;
            if current != state {
                let (_, record) = self.sessions()?.transition(session_id, state)?;
                self.journal()?.append_session(record);
            }
        }
        Ok(())
    }

    /// Observer installed on every runtime so events are journaled as they are
    /// produced rather than after the run finishes.
    fn run_observer(
        self: &Arc<Self>,
        handle: Weak<RunHandle>,
        session_id: AgentSessionId,
    ) -> AgentEventObserver {
        let backend = Arc::downgrade(self);
        Arc::new(move |event: &AgentEvent| {
            let Some(backend) = backend.upgrade() else {
                return;
            };
            let recorded = backend.record_agent_event(session_id, event.clone());
            if let Some(handle) = handle.upgrade() {
                handle.apply_event(event);
                if let Err(error) = recorded {
                    handle.record_failure(error);
                }
            }
        })
    }

    /// Wraps a runtime in a handle and attaches the journaling observer.
    fn register_runtime(self: &Arc<Self>, mut runtime: AgentRuntime) -> Arc<RunHandle> {
        let session_id = runtime.session_id();
        Arc::new_cyclic(|weak: &Weak<RunHandle>| {
            runtime.set_event_observer(self.run_observer(weak.clone(), session_id));
            RunHandle::new(runtime)
        })
    }

    /// Drives a registered run on its own worker so the request handler returns
    /// as soon as the run is registered.
    fn spawn_run_worker(self: &Arc<Self>, handle: Arc<RunHandle>) {
        handle.set_running(true);
        let backend = Arc::clone(self);
        std::thread::spawn(move || {
            loop {
                let progress = {
                    let mut runtime = handle
                        .runtime
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner);
                    let progress = runtime.run_step();
                    handle.refresh(&runtime);
                    progress
                };
                match progress {
                    Ok(progress) => {
                        if let Err(error) = backend.persist_state() {
                            handle.record_failure(error);
                            break;
                        }
                        if !progress.continues {
                            break;
                        }
                    }
                    Err(error) => {
                        handle.record_failure(error);
                        break;
                    }
                }
            }
            handle.set_running(false);
        });
    }

    fn cached_response(
        &self,
        request_id: loom_core::RequestId,
        request: &ClientRequest,
    ) -> Result<Option<ResponseEnvelope>> {
        let cache = self.idempotency()?;
        let Some(record) = cache.get(&request_id) else {
            return Ok(None);
        };
        if &record.request != request {
            return Err(LoomError::conflict(format!(
                "request id {request_id} was already used for a different mutation"
            )));
        }
        Ok(Some(record.response.clone()))
    }

    fn remember_response(
        &self,
        request_id: loom_core::RequestId,
        request: ClientRequest,
        response: ResponseEnvelope,
    ) -> Result<()> {
        let mut cache = self.idempotency()?;
        cache.insert(request_id, IdempotencyRecord { request, response });
        while cache.len() > IDEMPOTENCY_RETENTION {
            let Some(first) = cache.keys().next().copied() else {
                break;
            };
            cache.remove(&first);
        }
        Ok(())
    }

    pub fn set_event_retention(&self, limit: usize) -> Result<()> {
        if limit == 0 {
            return Err(LoomError::invalid_request(
                "event retention limit must be greater than zero",
            ));
        }
        self.journal()?.set_retention(limit);
        Ok(())
    }

    pub fn event_retention(&self) -> Result<usize> {
        Ok(self.journal()?.retention_limit.max(1))
    }
}

#[derive(Clone)]
pub struct InProcessConnection {
    backend: Arc<InProcessBackend>,
    negotiated_capabilities: Arc<Mutex<Option<CapabilitySet>>>,
    auth: Option<AuthSession>,
}

impl InProcessConnection {
    fn run_snapshot_projection(
        &self,
        run_id: loom_core::RunId,
    ) -> Result<AgentRunSnapshotProjection> {
        Ok(run_snapshot_projection(&self.run_handle(run_id)?.state()))
    }

    fn disk_resources(root: &Path) -> (Option<u64>, Option<u64>) {
        let Some(output) = std::process::Command::new("df")
            .args(["-kP", &root.to_string_lossy()])
            .output()
            .ok()
        else {
            return (None, None);
        };
        let output = String::from_utf8_lossy(&output.stdout).into_owned();
        let Some(line) = output.lines().nth(1) else {
            return (None, None);
        };
        let columns = line.split_whitespace().collect::<Vec<_>>();
        let Some(total) = columns
            .get(1)
            .and_then(|value| value.parse::<u64>().ok())
            .map(|value| value.saturating_mul(1024))
        else {
            return (None, None);
        };
        let Some(available) = columns
            .get(3)
            .and_then(|value| value.parse::<u64>().ok())
            .map(|value| value.saturating_mul(1024))
        else {
            return (None, None);
        };
        (Some(total), Some(available))
    }

    /// Looks a run up without touching its runtime lock.
    fn run_handle(&self, run_id: loom_core::RunId) -> Result<Arc<RunHandle>> {
        let handle = self
            .backend
            .runs()?
            .get(&run_id)
            .cloned()
            .ok_or_else(|| LoomError::not_found("agent run", run_id))?;
        if let Some(error) = handle.take_failure() {
            return Err(error);
        }
        Ok(handle)
    }

    fn session_snapshot_projection(
        &self,
        session_id: AgentSessionId,
    ) -> Result<AgentSessionSnapshotProjection> {
        let session = self.backend.sessions()?.get(session_id)?;
        let active_run = self
            .backend
            .runs()?
            .values()
            .filter(|handle| handle.session_id == session_id)
            .map(|handle| handle.state())
            .max_by_key(|state| state.run.updated_at)
            .as_ref()
            .map(run_snapshot_projection);
        let latest_sequence = self
            .backend
            .journal()?
            .events
            .iter()
            .filter(|event| event.session_id == session_id)
            .map(|event| event.sequence)
            .max()
            .unwrap_or_default();
        Ok(AgentSessionSnapshotProjection {
            session,
            active_run,
            latest_sequence,
        })
    }

    fn project_snapshots(&self) -> Result<Vec<ProjectSnapshot>> {
        let sessions = self.backend.sessions()?.list(None, true);
        let workspaces = self.backend.workspaces()?;
        let mut projects = BTreeMap::new();

        for (project_id, workspace) in workspaces.iter() {
            let name = workspace
                .root()
                .file_name()
                .and_then(|name| name.to_str())
                .filter(|name| !name.trim().is_empty())
                .unwrap_or("Project")
                .to_owned();
            projects.insert(
                *project_id,
                ProjectSnapshot {
                    id: *project_id,
                    name,
                    root: Some(workspace.root().display().to_string()),
                    session_count: 0,
                    updated_at: None,
                },
            );
        }
        drop(workspaces);

        for session in sessions {
            let project = projects
                .entry(session.project_id)
                .or_insert_with(|| ProjectSnapshot {
                    id: session.project_id,
                    name: format!("Project {}", session.project_id),
                    root: None,
                    session_count: 0,
                    updated_at: None,
                });
            project.session_count = project.session_count.saturating_add(1);
            project.updated_at = Some(project.updated_at.map_or(session.updated_at, |updated| {
                updated.max(session.updated_at)
            }));
        }
        Ok(projects.into_values().collect())
    }

    fn open_workspace(&self, project_id: ProjectId, root: String) -> Result<Workspace> {
        let requested_root = Workspace::canonical_root(&root)?;
        let mut workspaces = self.backend.workspaces()?;
        if let Some(existing) = workspaces.get(&project_id) {
            if existing.root() != requested_root {
                return Err(LoomError::conflict(format!(
                    "project {project_id} is already configured for workspace '{}'",
                    existing.root().display()
                )));
            }
            return Ok(existing.clone());
        }
        let candidate = Workspace::open(project_id, requested_root)?;
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

    fn vcs(&self, project_id: ProjectId) -> Result<GitService> {
        if let Some(service) = self.backend.vcs()?.get(&project_id).cloned() {
            return Ok(service);
        }
        let workspace = self.workspace(project_id)?;
        let service = GitService::open(workspace.root())?;
        self.backend.vcs()?.insert(project_id, service.clone());
        Ok(service)
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
        if let Some(auth) = &self.auth
            && let Err(error) = auth.verify()
        {
            return ResponseEnvelope::failure(request_id, error);
        }
        if !request
            .protocol_version
            .is_compatible_with(CURRENT_PROTOCOL_VERSION)
        {
            return ResponseEnvelope::failure(
                request_id,
                unsupported_version_error(request.protocol_version),
            );
        }

        let durable_mutation = request.request.is_retryable_mutation();
        let retryable = durable_mutation;
        let slot = if retryable {
            match self.backend.request_slot(request_id) {
                Ok(slot) => Some(slot),
                Err(error) => return ResponseEnvelope::failure(request_id, error),
            }
        } else {
            None
        };
        let _request_guard = slot
            .as_ref()
            .map(|slot| slot.lock().unwrap_or_else(PoisonError::into_inner));
        let request_for_cache = request.request.clone();
        if retryable {
            match self.backend.cached_response(request_id, &request_for_cache) {
                Ok(Some(response)) => return response,
                Ok(None) => {}
                Err(error) => return ResponseEnvelope::failure(request_id, error),
            }
        }

        let result = match request.request {
            ClientRequest::Negotiate {
                client_version,
                capabilities,
            } => self.negotiate(client_version, capabilities),
            ClientRequest::DiscoverCapabilities => self.discover_capabilities(),
            request => self.handle_after_negotiation(request),
        };
        let result = match result {
            Ok(response) => {
                if retryable {
                    let response_envelope = ResponseEnvelope::success(request_id, response.clone());
                    if let Err(error) = self.backend.remember_response(
                        request_id,
                        request_for_cache,
                        response_envelope,
                    ) {
                        return ResponseEnvelope::failure(request_id, error);
                    }
                }
                if durable_mutation {
                    self.backend.persist_state().map(|()| response)
                } else {
                    Ok(response)
                }
            }
            Err(error) => Err(error),
        };

        let response = match result {
            Ok(response) => ResponseEnvelope::success(request_id, response),
            Err(error) => ResponseEnvelope::failure(request_id, error),
        };
        if retryable {
            drop(_request_guard);
            drop(slot);
            self.backend.release_request_slot(request_id);
        }
        response
    }

    fn negotiate(
        &self,
        client_version: ProtocolVersion,
        capabilities: CapabilitySet,
    ) -> Result<ServerResponse> {
        if !client_version.is_compatible_with(CURRENT_PROTOCOL_VERSION) {
            return Err(unsupported_version_error(client_version));
        }
        let negotiated = capabilities
            .intersection(&self.backend.supported_capabilities)
            .intersection(&self.authorized_capabilities());
        *self.negotiated_capabilities()? = Some(negotiated.clone());

        Ok(ServerResponse::Negotiated(NegotiationResult {
            protocol_version: CURRENT_PROTOCOL_VERSION,
            capabilities: negotiated,
        }))
    }

    fn discover_capabilities(&self) -> Result<ServerResponse> {
        let capabilities = self
            .backend
            .supported_capabilities
            .intersection(&self.authorized_capabilities());
        Ok(ServerResponse::Capabilities(NegotiationResult {
            protocol_version: CURRENT_PROTOCOL_VERSION,
            capabilities,
        }))
    }

    fn handle_after_negotiation(&self, request: ClientRequest) -> Result<ServerResponse> {
        self.authorize_request(&request)?;
        let capabilities = self
            .negotiated_capabilities()?
            .clone()
            .ok_or_else(|| LoomError::invalid_request("connection must negotiate first"))?;
        if let Some(required) = request.required_capability()
            && !capabilities.contains(required)
        {
            return Err(LoomError::new(
                ErrorCode::CapabilityDenied,
                format!("connection did not negotiate capability {required:?}"),
                false,
            ));
        }

        match request {
            ClientRequest::Negotiate { .. } | ClientRequest::DiscoverCapabilities => {
                unreachable!("capability requests are handled above")
            }
            ClientRequest::GetWorkerNodeStatus => {
                Ok(ServerResponse::WorkerNodeStatus(self.worker_node_status()?))
            }
            ClientRequest::ListProjects => {
                let projects = self
                    .project_snapshots()?
                    .into_iter()
                    .filter(|project| {
                        self.auth
                            .as_ref()
                            .is_none_or(|auth| auth.scope().allows_project(project.id))
                    })
                    .collect();
                Ok(ServerResponse::Projects { projects })
            }
            ClientRequest::ListAgentSessions {
                project_id,
                include_archived,
            } => {
                let sessions = self
                    .backend
                    .sessions()?
                    .list(project_id, include_archived)
                    .into_iter()
                    .filter(|session| {
                        self.auth.as_ref().is_none_or(|auth| {
                            auth.scope().allows_project(session.project_id)
                                && auth.scope().allows_session(session.id)
                        })
                    })
                    .collect();
                Ok(ServerResponse::AgentSessions { sessions })
            }
            ClientRequest::CreateAgentSession { project_id, name } => {
                let (snapshot, record) = self.backend.sessions()?.create(project_id, name)?;
                self.backend.journal()?.append_session(record);
                Ok(ServerResponse::AgentSessionCreated(snapshot))
            }
            ClientRequest::GetAgentSession { session_id } => {
                let snapshot = self.backend.sessions()?.get(session_id)?;
                Ok(ServerResponse::AgentSession(snapshot))
            }
            ClientRequest::GetAgentSessionSnapshot { session_id } => Ok(
                ServerResponse::AgentSessionSnapshot(self.session_snapshot_projection(session_id)?),
            ),
            ClientRequest::RenameAgentSession { session_id, name } => {
                let (snapshot, record) = self.backend.sessions()?.rename(session_id, name)?;
                self.backend.journal()?.append_session(record);
                Ok(ServerResponse::AgentSessionRenamed(snapshot))
            }
            ClientRequest::ArchiveAgentSession { session_id } => self.archive_session(session_id),
            ClientRequest::GetSessionEvents {
                session_id,
                after_sequence,
            } => {
                let journal = self.backend.journal()?;
                let events = journal.events_since(session_id, after_sequence);
                let history_missing = session_id.is_some()
                    && after_sequence.is_none()
                    && !events.iter().any(|event| {
                        matches!(
                            &event.event,
                            loom_protocol::ServerEvent::AgentSessionCreated { .. }
                        )
                    });
                if let Some(session_id) = session_id
                    && (journal.is_cursor_stale(after_sequence) || history_missing)
                {
                    return Ok(ServerResponse::SessionEventsSnapshot {
                        session: self.backend.sessions()?.get(session_id)?,
                        events,
                        oldest_sequence: journal
                            .events
                            .first()
                            .map_or(EventSequence::default(), |event| event.sequence),
                        latest_sequence: journal.next_sequence,
                    });
                }
                Ok(ServerResponse::SessionEvents { events })
            }
            ClientRequest::GetRecentSessionEvents { session_id, limit } => {
                let journal = self.backend.journal()?;
                Ok(ServerResponse::SessionEvents {
                    events: journal.recent_events(session_id, limit as usize),
                })
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
            ClientRequest::GetAgentRun { run_id } => Ok(ServerResponse::AgentRun(
                self.run_handle(run_id)?.snapshot(),
            )),
            ClientRequest::GetAgentRunSnapshot { run_id } => Ok(ServerResponse::AgentRunSnapshot(
                self.run_snapshot_projection(run_id)?,
            )),
            ClientRequest::GetRunCheckpoint { run_id } => {
                let (project_id, checkpoint_id) = {
                    let handle = self.run_handle(run_id)?;
                    let session = self.backend.sessions()?.get(handle.session_id)?;
                    (
                        session.project_id,
                        handle.state().options.checkpoint_id.ok_or_else(|| {
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
            } => self.continue_run(run_id, |run| run.approve_entry(tool_call_id)),
            ClientRequest::RejectAgentAction {
                run_id,
                tool_call_id,
                reason,
            } => self.continue_run(run_id, |run| {
                run.reject(tool_call_id, reason).map(|events| RunProgress {
                    events,
                    continues: false,
                })
            }),
            ClientRequest::SendAgentMessage { run_id, message } => {
                self.continue_run(run_id, |run| run.message_entry(message))
            }
            ClientRequest::InterruptAgentRun { run_id } => {
                self.stop_run(run_id, RunStop::Interrupt)
            }
            ClientRequest::RetryAgentStep { run_id } => {
                self.continue_run(run_id, AgentRuntime::retry_entry)
            }
            ClientRequest::PauseAgentRun { run_id } => self.stop_run(run_id, RunStop::Pause),
            ClientRequest::ResumeAgentRun { run_id } => {
                self.continue_run(run_id, AgentRuntime::resume_entry)
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
                let state = self.run_handle(run_id)?.state();
                let provider = self
                    .backend
                    .providers
                    .usage()?
                    .summary(None, Some(&state.run.model));
                Ok(ServerResponse::RunUsage {
                    usage: state.usage,
                    provider,
                })
            }
            ClientRequest::GetSessionUsage { session_id } => {
                self.backend.sessions()?.get(session_id)?;
                let usage = self
                    .backend
                    .runs()?
                    .values()
                    .filter(|handle| handle.session_id == session_id)
                    .fold(loom_core::UsageSnapshot::default(), |mut total, handle| {
                        let current = handle.state().usage;
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
                let inspection = self
                    .run_handle(run_id)?
                    .state()
                    .context_inspection
                    .ok_or_else(|| {
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
            ClientRequest::GetWorkspaceConfig { project_id } => {
                Ok(ServerResponse::WorkspaceConfig(
                    self.backend
                        .workspace_configs()?
                        .get(&project_id)
                        .cloned()
                        .unwrap_or_default(),
                ))
            }
            ClientRequest::SetWorkspaceConfig { project_id, config } => {
                self.backend.set_workspace_config(project_id, config)?;
                Ok(ServerResponse::WorkspaceConfigUpdated)
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
            ClientRequest::GetWorkspaceChanges {
                project_id,
                after_sequence,
            } => {
                let mut changes = self.workspace(project_id)?.changes_since(after_sequence)?;
                let truncated = changes.len() > MAX_REVIEW_CHANGES;
                if changes.len() > MAX_REVIEW_CHANGES {
                    let start = changes.len() - MAX_REVIEW_CHANGES;
                    changes = changes.split_off(start);
                }
                Ok(ServerResponse::WorkspaceChanges { changes, truncated })
            }
            ClientRequest::ReadWorkspaceFile { project_id, path } => {
                let mut file = self.workspace(project_id)?.read_file(&path)?;
                file.content = bounded_review_text(&file.content, MAX_REVIEW_FILE_BYTES);
                Ok(ServerResponse::WorkspaceFile(file))
            }
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
            ClientRequest::ListTasks { project_id } => Ok(ServerResponse::Tasks {
                tasks: self.task_supervisor(project_id)?.list()?,
            }),
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
            ClientRequest::GetTaskEvidence {
                project_id,
                task_id,
            } => Ok(ServerResponse::TaskEvidence {
                evidence: self.task_supervisor(project_id)?.get(task_id)?.evidence,
            }),
            ClientRequest::GetContextFiles { project_id } => Ok(ServerResponse::ContextFiles {
                files: self.workspace(project_id)?.context_files()?,
            }),
            ClientRequest::GetVcsStatus { project_id } => {
                Ok(ServerResponse::VcsStatus(self.vcs(project_id)?.status()?))
            }
            ClientRequest::GetVcsDiff {
                project_id,
                path,
                staged,
            } => {
                let mut diff = self.vcs(project_id)?.diff(path.as_deref(), staged)?;
                diff.patch = bounded_review_text(&diff.patch, MAX_REVIEW_DIFF_BYTES);
                Ok(ServerResponse::VcsDiff(diff))
            }
            ClientRequest::GetVcsBranches { project_id } => Ok(ServerResponse::VcsBranches {
                branches: self.vcs(project_id)?.branches()?,
            }),
            ClientRequest::GetVcsConflicts { project_id } => Ok(ServerResponse::VcsConflicts {
                paths: self.vcs(project_id)?.conflicts()?,
            }),
            ClientRequest::AttachRunEvidence { run_id, evidence } => {
                let handle = self.run_handle(run_id)?;
                let mut runtime = handle.runtime_for_entry()?;
                runtime.add_evidence(evidence);
                handle.refresh(&runtime);
                Ok(ServerResponse::AgentRun(handle.snapshot()))
            }
        }
    }

    fn worker_node_status(&self) -> Result<WorkerNodeStatus> {
        let (disk_total_bytes, disk_available_bytes) = self
            .backend
            .workspaces()
            .ok()
            .and_then(|workspaces| {
                workspaces
                    .values()
                    .next()
                    .map(|workspace| Self::disk_resources(workspace.root()))
            })
            .unwrap_or((None, None));
        let resources = self
            .backend
            .resource_monitor()?
            .sample(disk_total_bytes, disk_available_bytes);
        Ok(WorkerNodeStatus {
            name: self.backend.node_name.clone(),
            node_id: self.backend.node_id.clone(),
            online: true,
            capabilities: self.backend.supported_capabilities.clone(),
            resources,
        })
    }

    fn authorize_request(&self, request: &ClientRequest) -> Result<()> {
        let Some(auth) = &self.auth else {
            return Ok(());
        };
        if let Some(capability) = request.required_capability()
            && !auth.scope().allows_capability(capability)
        {
            return Err(LoomError::new(
                ErrorCode::AuthorizationDenied,
                format!("token is not authorized for capability {capability:?}"),
                false,
            ));
        }

        let mut project_id = None;
        let mut session_id = None;
        let mut run_id = None;
        match request {
            ClientRequest::OpenWorkspace {
                project_id: requested_project,
                root,
            } => {
                project_id = Some(*requested_project);
                if !auth
                    .scope()
                    .allows_workspace_root(*requested_project, Path::new(root))
                {
                    return Err(LoomError::new(
                        ErrorCode::WorkspaceAccessDenied,
                        "token is not authorized for the requested workspace root",
                        false,
                    ));
                }
            }
            ClientRequest::ListAgentSessions {
                project_id: Some(requested_project),
                ..
            }
            | ClientRequest::CreateAgentSession {
                project_id: requested_project,
                ..
            }
            | ClientRequest::GetWorkspaceConfig {
                project_id: requested_project,
            }
            | ClientRequest::SetWorkspaceConfig {
                project_id: requested_project,
                ..
            }
            | ClientRequest::GetWorkspaceSnapshot {
                project_id: requested_project,
            }
            | ClientRequest::GetWorkspaceEvents {
                project_id: requested_project,
                ..
            }
            | ClientRequest::GetWorkspaceChanges {
                project_id: requested_project,
                ..
            }
            | ClientRequest::ReadWorkspaceFile {
                project_id: requested_project,
                ..
            }
            | ClientRequest::ApplyWorkspaceEdit {
                project_id: requested_project,
                ..
            }
            | ClientRequest::TakeWorkspaceControl {
                project_id: requested_project,
                ..
            }
            | ClientRequest::RevertCheckpoint {
                project_id: requested_project,
                ..
            }
            | ClientRequest::UndoWorkspaceEdit {
                project_id: requested_project,
            }
            | ClientRequest::SetApprovalPolicy {
                project_id: requested_project,
                ..
            }
            | ClientRequest::OpenTerminal {
                project_id: requested_project,
                ..
            }
            | ClientRequest::WriteTerminalInput {
                project_id: requested_project,
                ..
            }
            | ClientRequest::ResizeTerminal {
                project_id: requested_project,
                ..
            }
            | ClientRequest::GetTerminalEvents {
                project_id: requested_project,
                ..
            }
            | ClientRequest::CancelTerminal {
                project_id: requested_project,
                ..
            }
            | ClientRequest::StartTask {
                project_id: requested_project,
                ..
            }
            | ClientRequest::ListTasks {
                project_id: requested_project,
            }
            | ClientRequest::GetTask {
                project_id: requested_project,
                ..
            }
            | ClientRequest::GetTaskEvents {
                project_id: requested_project,
                ..
            }
            | ClientRequest::CancelTask {
                project_id: requested_project,
                ..
            } => project_id = Some(*requested_project),
            ClientRequest::GetContextFiles {
                project_id: requested_project,
            }
            | ClientRequest::GetVcsStatus {
                project_id: requested_project,
            }
            | ClientRequest::GetVcsDiff {
                project_id: requested_project,
                ..
            }
            | ClientRequest::GetVcsBranches {
                project_id: requested_project,
            }
            | ClientRequest::GetVcsConflicts {
                project_id: requested_project,
            }
            | ClientRequest::GetTaskEvidence {
                project_id: requested_project,
                ..
            } => project_id = Some(*requested_project),
            ClientRequest::GetAgentSession {
                session_id: requested_session,
            }
            | ClientRequest::GetAgentSessionSnapshot {
                session_id: requested_session,
            }
            | ClientRequest::RenameAgentSession {
                session_id: requested_session,
                ..
            }
            | ClientRequest::ArchiveAgentSession {
                session_id: requested_session,
            }
            | ClientRequest::GetSessionUsage {
                session_id: requested_session,
            }
            | ClientRequest::ForkAgentSession {
                session_id: requested_session,
                ..
            } => session_id = Some(*requested_session),
            ClientRequest::GetSessionEvents {
                session_id: requested_session,
                ..
            } => session_id = *requested_session,
            ClientRequest::GetRecentSessionEvents {
                session_id: requested_session,
                ..
            } => session_id = Some(*requested_session),
            ClientRequest::StartAgentRun {
                session_id: requested_session,
                ..
            }
            | ClientRequest::StartAgentRunWithOptions {
                session_id: requested_session,
                ..
            } => session_id = Some(*requested_session),
            ClientRequest::GetAgentRun {
                run_id: requested_run,
            }
            | ClientRequest::GetAgentRunSnapshot {
                run_id: requested_run,
            }
            | ClientRequest::GetRunCheckpoint {
                run_id: requested_run,
            }
            | ClientRequest::ApproveAgentAction {
                run_id: requested_run,
                ..
            }
            | ClientRequest::RejectAgentAction {
                run_id: requested_run,
                ..
            }
            | ClientRequest::InterruptAgentRun {
                run_id: requested_run,
            }
            | ClientRequest::RetryAgentStep {
                run_id: requested_run,
            }
            | ClientRequest::PauseAgentRun {
                run_id: requested_run,
            }
            | ClientRequest::ResumeAgentRun {
                run_id: requested_run,
            }
            | ClientRequest::RetryAgentFromCheckpoint {
                run_id: requested_run,
                ..
            }
            | ClientRequest::GetRunUsage {
                run_id: requested_run,
            }
            | ClientRequest::InspectAgentContext {
                run_id: requested_run,
            } => run_id = Some(*requested_run),
            ClientRequest::SendAgentMessage {
                run_id: requested_run,
                ..
            } => run_id = Some(*requested_run),
            ClientRequest::AttachRunEvidence {
                run_id: requested_run,
                ..
            } => run_id = Some(*requested_run),
            ClientRequest::CreateCheckpoint {
                project_id: requested_project,
                session_id: Some(requested_session),
                ..
            } => {
                project_id = Some(*requested_project);
                session_id = Some(*requested_session);
            }
            ClientRequest::Negotiate { .. }
            | ClientRequest::DiscoverCapabilities
            | ClientRequest::ListProjects
            | ClientRequest::ListAgentSessions {
                project_id: None, ..
            }
            | ClientRequest::ListModels
            | ClientRequest::GetWorkerNodeStatus
            | ClientRequest::ListProviders
            | ClientRequest::DiscoverProviderModels { .. }
            | ClientRequest::GetProviderHealth { .. } => {}
            ClientRequest::CreateCheckpoint {
                session_id: None, ..
            } => {}
        }

        if let Some(session_id) = session_id {
            if !auth.scope().allows_session(session_id) {
                return Err(unauthorized_session(session_id));
            }
            let session_project = self.backend.sessions()?.get(session_id)?.project_id;
            if project_id.is_some_and(|requested_project| requested_project != session_project) {
                return Err(LoomError::new(
                    ErrorCode::WorkspaceAccessDenied,
                    "session does not belong to the requested workspace",
                    false,
                ));
            }
            project_id = Some(session_project);
        } else if run_id.is_none()
            && project_id.is_none()
            && matches!(
                request,
                ClientRequest::GetSessionEvents {
                    session_id: None,
                    ..
                }
            )
            && (auth.scope().projects.is_some() || auth.scope().sessions.is_some())
        {
            return Err(LoomError::new(
                ErrorCode::AuthorizationDenied,
                "an unrestricted token is required to list events across sessions",
                false,
            ));
        }

        if let Some(run_id) = run_id {
            let session_id = self
                .backend
                .runs()?
                .get(&run_id)
                .ok_or_else(|| LoomError::not_found("agent run", run_id))?
                .session_id;
            if !auth.scope().allows_session(session_id) {
                return Err(unauthorized_session(session_id));
            }
            project_id = Some(self.backend.sessions()?.get(session_id)?.project_id);
        }

        if let Some(project_id) = project_id
            && !auth.scope().allows_project(project_id)
        {
            return Err(unauthorized_project(project_id));
        }
        Ok(())
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
        let admission = self.backend.session_admission(input.session_id)?;
        let _admission_guard = admission.try_lock().map_err(|_| {
            LoomError::conflict("another agent run is already being started for this session")
        })?;
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
        if input.repository_instructions.is_none() {
            let instructions = workspace.instruction_text()?;
            if !instructions.trim().is_empty() {
                input.repository_instructions = Some(instructions);
            }
        }
        let checkpoint = workspace.create_checkpoint(Some(input.session_id), "before agent run")?;
        input.options.checkpoint_id = Some(checkpoint.id);
        let tools = ToolExecutor::new_with_workspace(workspace);
        let policy = self.policy(session.project_id)?;
        let mut agent_task = AgentTask::new(input.task, input.model)?;
        agent_task.system_instructions = input.system_instructions;
        agent_task.repository_instructions = input.repository_instructions;
        let runtime = AgentRuntime::new_with_policy_and_options(
            input.session_id,
            agent_task,
            provider,
            tools,
            policy,
            input.options,
        );
        let run_id = runtime.run_id();
        // The run is registered, and its events observable, before any model
        // work starts, so a second client can control it immediately.
        let handle = self.backend.register_runtime(runtime);
        self.backend.runs()?.insert(run_id, Arc::clone(&handle));
        let progress = {
            let mut runtime = handle.try_runtime()?;
            let progress = runtime.begin();
            handle.refresh(&runtime);
            progress?
        };
        if progress.continues {
            self.backend.spawn_run_worker(Arc::clone(&handle));
        }
        Ok(ServerResponse::AgentRunStarted(handle.snapshot()))
    }

    /// Applies an operation that may leave the run with more work, then hands
    /// the remaining work to the run worker instead of the request handler.
    fn continue_run(
        &self,
        run_id: loom_core::RunId,
        operation: impl FnOnce(&mut AgentRuntime) -> Result<RunProgress>,
    ) -> Result<ServerResponse> {
        let handle = self.run_handle(run_id)?;
        let progress = {
            let mut runtime = handle.runtime_for_entry()?;
            let progress = operation(&mut runtime);
            handle.refresh(&runtime);
            progress?
        };
        if progress.continues {
            self.backend.spawn_run_worker(Arc::clone(&handle));
        }
        Ok(ServerResponse::AgentRun(handle.snapshot()))
    }

    /// Pauses or interrupts a run. The request only raises the control flag, so
    /// it is never queued behind the model call it is stopping.
    fn stop_run(&self, run_id: loom_core::RunId, stop: RunStop) -> Result<ServerResponse> {
        let handle = self.run_handle(run_id)?;
        if handle.is_running() {
            match stop {
                RunStop::Interrupt => handle.control.request_interrupt(),
                RunStop::Pause => handle.control.request_pause(),
            }
            handle.wait_until_idle()?;
            if let Some(error) = handle.take_failure() {
                return Err(error);
            }
            if handle.control.is_stopping() {
                let state = handle.state().run.state;
                if !matches!(
                    state,
                    AgentRunState::Completed | AgentRunState::Failed | AgentRunState::Cancelled
                ) {
                    let mut runtime = handle.try_runtime()?;
                    let result = match stop {
                        RunStop::Interrupt => runtime.interrupt(),
                        RunStop::Pause => runtime.pause(),
                    };
                    handle.refresh(&runtime);
                    result?;
                    self.backend.persist_state()?;
                }
                handle.control.clear_request();
            }
            return Ok(ServerResponse::AgentRun(handle.snapshot()));
        }
        let mut runtime = handle.runtime_for_entry()?;
        let result = match stop {
            RunStop::Interrupt => runtime.interrupt(),
            RunStop::Pause => runtime.pause(),
        };
        handle.refresh(&runtime);
        result?;
        Ok(ServerResponse::AgentRun(handle.snapshot()))
    }

    fn archive_session(&self, session_id: AgentSessionId) -> Result<ServerResponse> {
        let session = self.backend.sessions()?.get(session_id)?;
        if matches!(
            session.state,
            AgentSessionState::Queued
                | AgentSessionState::Planning
                | AgentSessionState::AwaitingApproval
                | AgentSessionState::Paused
                | AgentSessionState::Executing
                | AgentSessionState::Evaluating
                | AgentSessionState::NeedsInput
        ) {
            let run_id = self
                .backend
                .runs()?
                .iter()
                .filter(|(_, handle)| handle.session_id == session_id)
                .map(|(run_id, handle)| (*run_id, handle.snapshot()))
                .filter(|(_, snapshot)| {
                    !matches!(
                        snapshot.state,
                        AgentRunState::Completed | AgentRunState::Failed | AgentRunState::Cancelled
                    )
                })
                .max_by_key(|(_, snapshot)| snapshot.updated_at)
                .map(|(run_id, _)| run_id)
                .ok_or_else(|| {
                    LoomError::invalid_state(
                        "running agent sessions must be stopped before archiving",
                    )
                })?;
            self.stop_run(run_id, RunStop::Interrupt)?;
        }

        let (snapshot, record) = self.backend.sessions()?.archive(session_id)?;
        self.backend.journal()?.append_session(record);
        Ok(ServerResponse::AgentSessionArchived(snapshot))
    }

    fn retry_from_checkpoint(
        &self,
        run_id: loom_core::RunId,
        checkpoint_id: loom_core::CheckpointId,
    ) -> Result<ServerResponse> {
        let handle = self.run_handle(run_id)?;
        let project_id = self.backend.sessions()?.get(handle.session_id)?.project_id;
        if handle.state().options.checkpoint_id != Some(checkpoint_id) {
            return Err(LoomError::conflict(format!(
                "checkpoint {checkpoint_id} is not the checkpoint associated with run {run_id}"
            )));
        }
        self.workspace(project_id)?
            .revert_checkpoint(checkpoint_id)?;
        self.continue_run(run_id, AgentRuntime::checkpoint_retry_entry)
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
        AgentRunState::NeedsInput => AgentSessionState::NeedsInput,
        AgentRunState::Completed => AgentSessionState::Completed,
        AgentRunState::Failed => AgentSessionState::Failed,
        AgentRunState::Cancelled => AgentSessionState::Cancelled,
    })
}

fn bounded_review_text(value: &str, limit: usize) -> String {
    if value.len() <= limit {
        return value.to_owned();
    }
    let end = value
        .char_indices()
        .map(|(index, character)| (index, index + character.len_utf8()))
        .take_while(|(_, end)| *end <= limit)
        .map(|(_, end)| end)
        .last()
        .unwrap_or_default();
    let mut result = value[..end].to_owned();
    result.push_str("\n...[review output truncated]");
    result
}

fn run_snapshot_projection(state: &AgentRuntimeState) -> AgentRunSnapshotProjection {
    let mut messages = state.messages.clone();
    for message in &mut messages {
        message.content = bounded_review_text(&message.content, MAX_RUN_MESSAGE_BYTES);
    }
    let mut run = state.run.clone();
    if let Some(summary) = &mut run.summary {
        *summary = bounded_review_text(summary, MAX_RUN_MESSAGE_BYTES);
    }
    AgentRunSnapshotProjection {
        run,
        plan: state.plan.steps.clone(),
        messages,
        pending_approval: state.pending_approval.clone(),
        pending_input: state.pending_input.clone(),
        usage: state.usage.clone(),
        activities: state.activities.clone(),
    }
}

fn unauthorized_project(project_id: ProjectId) -> LoomError {
    LoomError::new(
        ErrorCode::AuthorizationDenied,
        format!("token is not authorized for project {project_id}"),
        false,
    )
}

fn unauthorized_session(session_id: AgentSessionId) -> LoomError {
    LoomError::new(
        ErrorCode::AuthorizationDenied,
        format!("token is not authorized for session {session_id}"),
        false,
    )
}

impl InProcessConnection {
    pub fn disconnected(backend: Arc<InProcessBackend>) -> Self {
        Self {
            backend,
            negotiated_capabilities: Arc::new(Mutex::new(None)),
            auth: None,
        }
    }

    fn authorized_capabilities(&self) -> CapabilitySet {
        self.auth
            .as_ref()
            .and_then(|auth| auth.scope().capabilities.clone())
            .unwrap_or_else(|| self.backend.supported_capabilities.clone())
    }
}

#[cfg(test)]
mod tests {
    use std::{fs, path::PathBuf, process::Command, thread, time::Duration};

    use loom_context::ContextAssemblyOptions;
    use loom_core::{CapabilitySet, PolicyDecision, ProjectId, ToolCallId};
    use loom_process::{TaskEvent, TaskKind, TaskSpec, TaskStatus, TerminalEvent};
    use loom_protocol::{
        AgentActivityStatus, ClientRequest, RequestEnvelope, ServerEvent, ServerResponse,
        WorkerNodeConfig, WorkspaceConfig,
    };
    use loom_workspace::{WorkspaceControl, WorkspaceEdit};

    use super::*;

    fn negotiate(connection: &InProcessConnection) {
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

    fn negotiate_m5(connection: &InProcessConnection) {
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
                Capability::ApproveAgentAction,
                Capability::OpenWorkspace,
                Capability::ReadWorkspace,
                Capability::WriteWorkspace,
                Capability::ReadWorkspaceInstructions,
                Capability::ReadVcsStatus,
                Capability::ReadVcsDiff,
                Capability::ReadTask,
                Capability::StartTask,
                Capability::ReadTaskEvidence,
                Capability::ReadWorkerNodeStatus,
            ]),
        }));
        assert!(matches!(response.result, Ok(ServerResponse::Negotiated(_))));
    }

    #[test]
    fn worker_node_status_reports_capabilities_and_resources() {
        let backend = InProcessBackend::new();
        let connection = backend.connect();
        negotiate_m5(&connection);
        #[cfg(unix)]
        let project_id = ProjectId::new();
        #[cfg(unix)]
        let disk_root = {
            let root = workspace();
            let opened = connection.request(RequestEnvelope::new(ClientRequest::OpenWorkspace {
                project_id,
                root: root.display().to_string(),
            }));
            assert!(matches!(
                opened.result,
                Ok(ServerResponse::WorkspaceOpened(_))
            ));
            root
        };
        let response = connection.request(RequestEnvelope::new(ClientRequest::GetWorkerNodeStatus));
        let response =
            loom_protocol::decode_response(&loom_protocol::encode_response(&response).unwrap())
                .unwrap();
        let ServerResponse::WorkerNodeStatus(status) = response.result.unwrap() else {
            panic!("expected worker node status");
        };
        assert!(status.online);
        assert!(status.resources.cpu_count > 0);
        assert_eq!(status.resources.cpu_usage_percent, None);
        assert!(
            status
                .resources
                .memory_total_bytes
                .is_some_and(|bytes| bytes > 0)
        );
        assert!(status.resources.memory_available_bytes.is_some());
        assert!(
            status
                .resources
                .memory_usage_percent
                .is_some_and(|value| value <= 100)
        );
        #[cfg(unix)]
        {
            assert!(status.resources.disk_total_bytes.is_some());
            assert!(status.resources.disk_available_bytes.is_some());
        }

        std::thread::sleep(Duration::from_millis(250));
        let refreshed =
            connection.request(RequestEnvelope::new(ClientRequest::GetWorkerNodeStatus));
        let refreshed =
            loom_protocol::decode_response(&loom_protocol::encode_response(&refreshed).unwrap())
                .unwrap();
        let ServerResponse::WorkerNodeStatus(refreshed) = refreshed.result.unwrap() else {
            panic!("expected refreshed worker node status");
        };
        assert!(
            refreshed
                .resources
                .cpu_usage_percent
                .is_some_and(|value| value <= 100)
        );
        assert!(
            refreshed
                .resources
                .memory_total_bytes
                .is_some_and(|bytes| bytes > 0)
        );
        assert!(
            refreshed
                .resources
                .memory_usage_percent
                .is_some_and(|value| value <= 100)
        );
        assert_eq!(refreshed.resources.cpu_count, status.resources.cpu_count);
        assert_eq!(refreshed.node_id, status.node_id);
        assert_eq!(refreshed.name, status.name);
        assert_eq!(
            refreshed.resources.memory_total_bytes,
            status.resources.memory_total_bytes
        );
        #[cfg(unix)]
        {
            assert!(refreshed.resources.disk_total_bytes.is_some());
            assert!(refreshed.resources.disk_available_bytes.is_some());
            fs::remove_dir_all(disk_root).unwrap();
        }
        assert!(status.capabilities.contains(Capability::ReadAgentSession));
    }

    #[test]
    fn worker_resource_percentages_handle_unavailable_and_out_of_range_samples() {
        assert_eq!(cpu_usage_percent(f32::NAN), None);
        assert_eq!(cpu_usage_percent(-1.0), Some(0));
        assert_eq!(cpu_usage_percent(47.6), Some(48));
        assert_eq!(cpu_usage_percent(120.0), Some(100));
        assert_eq!(memory_usage_percent(None, Some(5)), None);
        assert_eq!(memory_usage_percent(Some(0), Some(0)), None);
        assert_eq!(memory_usage_percent(Some(100), Some(25)), Some(75));
        assert_eq!(memory_usage_percent(Some(100), Some(150)), Some(0));
    }

    #[test]
    fn worker_resource_monitor_measures_cpu_utilization_after_a_baseline_sample() {
        let mut monitor = ResourceMonitor::default();

        assert_eq!(monitor.sample(None, None).cpu_usage_percent, None);
        std::thread::sleep(Duration::from_millis(250));
        assert!(
            monitor
                .sample(None, None)
                .cpu_usage_percent
                .is_some_and(|value| value <= 100)
        );
    }

    #[test]
    fn workspace_config_is_persisted_and_excludes_access_tokens() {
        let path =
            std::env::temp_dir().join(format!("loom-workspace-config-{}.db", ProjectId::new()));
        let project_id = ProjectId::new();
        let config = WorkspaceConfig {
            revision: 1,
            cpu_pulse_threshold_percent: 37,
            worker_nodes: vec![WorkerNodeConfig {
                url: "wss://worker.example/ws".to_owned(),
            }],
        };
        {
            let backend = InProcessBackend::new_persistent(&path).unwrap();
            let connection = backend.connect();
            negotiate_m5(&connection);
            let response =
                connection.request(RequestEnvelope::new(ClientRequest::SetWorkspaceConfig {
                    project_id,
                    config: config.clone(),
                }));
            assert!(matches!(
                response.result,
                Ok(ServerResponse::WorkspaceConfigUpdated)
            ));
        }

        {
            let backend = InProcessBackend::new_persistent(&path).unwrap();
            let connection = backend.connect();
            negotiate_m5(&connection);
            let response =
                connection.request(RequestEnvelope::new(ClientRequest::GetWorkspaceConfig {
                    project_id,
                }));
            assert!(matches!(
                response.result,
                Ok(ServerResponse::WorkspaceConfig(saved)) if saved == config
            ));
            let response =
                connection.request(RequestEnvelope::new(ClientRequest::SetWorkspaceConfig {
                    project_id,
                    config: WorkspaceConfig {
                        revision: 0,
                        cpu_pulse_threshold_percent: 5,
                        worker_nodes: vec![WorkerNodeConfig {
                            url: "wss://stale.example/ws".to_owned(),
                        }],
                    },
                }));
            assert!(matches!(
                response.result,
                Ok(ServerResponse::WorkspaceConfigUpdated)
            ));
            let response =
                connection.request(RequestEnvelope::new(ClientRequest::GetWorkspaceConfig {
                    project_id,
                }));
            assert!(matches!(
                response.result,
                Ok(ServerResponse::WorkspaceConfig(saved)) if saved == config
            ));
            for url in [
                "wss://worker.example/ws?%61ccess_token=secret",
                "wss://user:secret@worker.example/ws",
            ] {
                let response =
                    connection.request(RequestEnvelope::new(ClientRequest::SetWorkspaceConfig {
                        project_id,
                        config: WorkspaceConfig {
                            revision: 2,
                            cpu_pulse_threshold_percent: 5,
                            worker_nodes: vec![WorkerNodeConfig {
                                url: url.to_owned(),
                            }],
                        },
                    }));
                assert!(response.result.is_err());
            }
        }
        std::fs::remove_file(path).unwrap();
    }

    /// Waits until a run stops needing the model, because a run is now driven by
    /// its own worker rather than by the request that started it.
    fn await_settled_run(
        connection: &InProcessConnection,
        run_id: loom_core::RunId,
    ) -> loom_agent::AgentRunSnapshot {
        for _ in 0..1_000 {
            let response =
                connection.request(RequestEnvelope::new(ClientRequest::GetAgentRun { run_id }));
            let Ok(ServerResponse::AgentRun(snapshot)) = response.result else {
                panic!("unexpected run response");
            };
            if !matches!(
                snapshot.state,
                AgentRunState::Planning | AgentRunState::Executing | AgentRunState::Evaluating
            ) {
                return snapshot;
            }
            thread::sleep(Duration::from_millis(5));
        }
        panic!("agent run did not settle");
    }

    fn workspace() -> PathBuf {
        let root = std::env::temp_dir().join(format!("loom-server-{}", ProjectId::new()));
        fs::create_dir(&root).unwrap();
        root
    }

    #[test]
    fn m5_session_projections_reconnect_and_archive_authoritatively() {
        let root = workspace();
        fs::write(root.join("README.md"), "M5 session projection\n").unwrap();
        let backend = InProcessBackend::new();
        let connection = backend.connect();
        negotiate_m5(&connection);
        let project_id = ProjectId::new();

        let opened = connection.request(RequestEnvelope::new(ClientRequest::OpenWorkspace {
            project_id,
            root: root.display().to_string(),
        }));
        assert!(matches!(
            opened.result,
            Ok(ServerResponse::WorkspaceOpened(_))
        ));

        let created = connection.request(RequestEnvelope::new(ClientRequest::CreateAgentSession {
            project_id,
            name: "Navigator session".to_owned(),
        }));
        let session = match created.result.unwrap() {
            ServerResponse::AgentSessionCreated(snapshot) => snapshot,
            response => panic!("unexpected response: {response:?}"),
        };

        let projects = connection.request(RequestEnvelope::new(ClientRequest::ListProjects));
        let ServerResponse::Projects { projects } = projects.result.unwrap() else {
            panic!("unexpected project response");
        };
        assert_eq!(projects.len(), 1);
        assert_eq!(projects[0].session_count, 1);

        let renamed = connection.request(RequestEnvelope::new(ClientRequest::RenameAgentSession {
            session_id: session.id,
            name: "Renamed session".to_owned(),
        }));
        let session = match renamed.result.unwrap() {
            ServerResponse::AgentSessionRenamed(snapshot) => snapshot,
            response => panic!("unexpected rename response: {response:?}"),
        };
        assert_eq!(session.name, "Renamed session");

        let started = connection.request(RequestEnvelope::new(ClientRequest::StartAgentRun {
            session_id: session.id,
            task: "inspect the workspace".to_owned(),
            model: loom_model::ModelId::new("deterministic/demo"),
            workspace_root: root.display().to_string(),
            system_instructions: None,
            repository_instructions: None,
        }));
        let run_id = match started.result.unwrap() {
            ServerResponse::AgentRunStarted(snapshot) => snapshot.id,
            response => panic!("unexpected run response: {response:?}"),
        };

        let snapshot = connection.request(RequestEnvelope::new(
            ClientRequest::GetAgentSessionSnapshot {
                session_id: session.id,
            },
        ));
        let ServerResponse::AgentSessionSnapshot(snapshot) = snapshot.result.unwrap() else {
            panic!("unexpected session snapshot response");
        };
        assert_eq!(snapshot.session.id, session.id);
        assert_eq!(
            snapshot.active_run.as_ref().map(|run| run.run.id),
            Some(run_id)
        );
        assert!(snapshot.active_run.unwrap().plan.is_empty());

        let run = connection.request(RequestEnvelope::new(ClientRequest::GetAgentRunSnapshot {
            run_id,
        }));
        let ServerResponse::AgentRunSnapshot(run) = run.result.unwrap() else {
            panic!("unexpected run snapshot response");
        };
        assert_eq!(run.run.id, run_id);
        assert!(!run.messages.is_empty());

        let changes =
            connection.request(RequestEnvelope::new(ClientRequest::GetWorkspaceChanges {
                project_id,
                after_sequence: None,
            }));
        assert!(matches!(
            changes.result,
            Ok(ServerResponse::WorkspaceChanges { .. })
        ));

        let archived =
            connection.request(RequestEnvelope::new(ClientRequest::ArchiveAgentSession {
                session_id: session.id,
            }));
        assert!(matches!(
            archived.result,
            Ok(ServerResponse::AgentSessionArchived(_))
        ));
        let sessions = connection.request(RequestEnvelope::new(ClientRequest::ListAgentSessions {
            project_id: Some(project_id),
            include_archived: false,
        }));
        let ServerResponse::AgentSessions { sessions } = sessions.result.unwrap() else {
            panic!("unexpected session list response");
        };
        assert!(sessions.is_empty());
        fs::remove_dir_all(root).unwrap();
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
        let history = connection.request(RequestEnvelope::new(ClientRequest::GetSessionEvents {
            session_id: Some(session_id),
            after_sequence: None,
        }));
        let ServerResponse::SessionEvents { events } = history.result.unwrap() else {
            panic!("unexpected history response");
        };
        assert!(events.iter().any(|event| {
            matches!(
                &event.event,
                ServerEvent::Agent {
                    event: AgentEvent::ActivityRecorded { activity, .. }
                } if activity.run_id == run_id && activity.completed_at.is_some()
            )
        }));
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
        let listed = connection.request(RequestEnvelope::new(ClientRequest::ListTasks {
            project_id,
        }));
        let ServerResponse::Tasks { tasks } = listed.result.unwrap() else {
            panic!("unexpected task list response");
        };
        assert!(tasks.iter().any(|task| task.id == task_id));
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
        await_settled_run(&connection, run_id);
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
        assert_eq!(snapshot.state, AgentRunState::AwaitingApproval);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn persistent_backend_recovers_transcript_workspace_and_pending_approval() {
        let root = workspace();
        let persistence =
            std::env::temp_dir().join(format!("loom-server-state-{}.db", ProjectId::new()));
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
            await_settled_run(&connection, run_id);
            backend.flush().unwrap();
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
        let recovered_snapshot =
            connection.request(RequestEnvelope::new(ClientRequest::GetAgentRunSnapshot {
                run_id,
            }));
        let ServerResponse::AgentRunSnapshot(projection) = recovered_snapshot.result.unwrap()
        else {
            panic!("unexpected run snapshot response");
        };
        assert!(!projection.activities.is_empty());
        assert!(
            projection
                .activities
                .iter()
                .any(|activity| activity.status == AgentActivityStatus::AwaitingApproval)
        );
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
        await_settled_run(&connection, run_id);
        let command_approval = (0..1_000)
            .find_map(|_| {
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
                let approval = events.iter().find_map(|event| match &event.event {
                    ServerEvent::Agent {
                        event: AgentEvent::ToolApprovalRequired { call, .. },
                    } if call.name == "run_command" => Some(call.id),
                    _ => None,
                });
                approval.or_else(|| {
                    thread::sleep(Duration::from_millis(5));
                    None
                })
            })
            .expect("run_command approval did not arrive");
        let response =
            connection.request(RequestEnvelope::new(ClientRequest::ApproveAgentAction {
                run_id,
                tool_call_id: command_approval,
            }));
        assert!(response.result.is_ok());
        let mut usage = match connection
            .request(RequestEnvelope::new(ClientRequest::GetRunUsage { run_id }))
            .result
            .unwrap()
        {
            ServerResponse::RunUsage { usage, .. } => usage,
            response => panic!("unexpected usage response: {response:?}"),
        };
        for _ in 0..1_000 {
            if usage.input_tokens > 0 {
                break;
            }
            thread::sleep(Duration::from_millis(5));
            usage = match connection
                .request(RequestEnvelope::new(ClientRequest::GetRunUsage { run_id }))
                .result
                .unwrap()
            {
                ServerResponse::RunUsage { usage, .. } => usage,
                response => panic!("unexpected usage response: {response:?}"),
            };
        }
        assert_eq!(usage.input_tokens, 240);
        assert_eq!(usage.output_tokens, 52);
        assert_eq!(usage.tool_calls, 3);
        let workspace_snapshot =
            connection.request(RequestEnvelope::new(ClientRequest::GetWorkspaceSnapshot {
                project_id,
            }));
        assert!(workspace_snapshot.result.is_ok());
        backend.flush().unwrap();
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
        assert!(matches!(retried.result, Ok(ServerResponse::AgentRun(_))));
        assert_eq!(
            await_settled_run(&reopened_connection, run_id).state,
            AgentRunState::AwaitingApproval
        );
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
        await_settled_run(&connection, run_id);
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
            ServerResponse::AgentRunStarted(snapshot) => snapshot.id,
            response => panic!("unexpected response: {response:?}"),
        };
        assert_eq!(
            await_settled_run(&connection, run_id).state,
            AgentRunState::Failed
        );
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
    fn non_sqlite_persistence_file_is_rejected_without_fallback() {
        let path =
            std::env::temp_dir().join(format!("loom-server-malformed-{}.db", ProjectId::new()));
        fs::write(&path, br#"{"schema_version":1,"state":{"broken":true}}"#).unwrap();
        let error = match InProcessBackend::new_persistent(&path) {
            Ok(_) => panic!("non-SQLite persistence unexpectedly loaded"),
            Err(error) => error,
        };
        assert_eq!(error.code, ErrorCode::Persistence);
        assert!(!path.with_extension("json.legacy").exists());
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn m5_workspace_context_vcs_and_task_evidence_are_authoritative() {
        let root = workspace();
        fs::write(root.join("README.md"), "fn answer() {\n TODO\n}\n").unwrap();
        let git = |arguments: &[&str]| {
            assert!(
                Command::new("git")
                    .env_remove("GIT_DIR")
                    .env_remove("GIT_WORK_TREE")
                    .env_remove("GIT_INDEX_FILE")
                    .env_remove("GIT_COMMON_DIR")
                    .args(arguments)
                    .current_dir(&root)
                    .status()
                    .unwrap()
                    .success()
            );
        };
        git(&["init", "-q"]);
        git(&["config", "user.email", "loom@example.test"]);
        git(&["config", "user.name", "Loom Test"]);
        git(&["add", "--", "README.md"]);
        git(&["commit", "-qm", "initial"]);

        let backend = InProcessBackend::new();
        let connection = backend.connect();
        negotiate_m5(&connection);
        let project_id = ProjectId::new();
        let opened = connection.request(RequestEnvelope::new(ClientRequest::OpenWorkspace {
            project_id,
            root: root.display().to_string(),
        }));
        assert!(matches!(
            opened.result,
            Ok(ServerResponse::WorkspaceOpened(_))
        ));

        let context = connection.request(RequestEnvelope::new(ClientRequest::GetContextFiles {
            project_id,
        }));
        assert!(matches!(
            context.result,
            Ok(ServerResponse::ContextFiles { .. })
        ));
        let vcs = connection.request(RequestEnvelope::new(ClientRequest::GetVcsStatus {
            project_id,
        }));
        assert!(matches!(vcs.result, Ok(ServerResponse::VcsStatus(_))));

        let task = connection.request(RequestEnvelope::new(ClientRequest::StartTask {
            project_id,
            spec: TaskSpec {
                kind: TaskKind::Test,
                label: "evidence fixture".to_owned(),
                command: if cfg!(windows) {
                    "cmd".to_owned()
                } else {
                    "printf".to_owned()
                },
                args: if cfg!(windows) {
                    vec!["/C".to_owned(), "ok".to_owned()]
                } else {
                    vec!["ok".to_owned()]
                },
                cwd: None,
                output_limit_bytes: Some(128),
                artifact_paths: Vec::new(),
            },
        }));
        let task_id = match task.result.unwrap() {
            ServerResponse::TaskStarted(task) => task.id,
            response => panic!("unexpected task response: {response:?}"),
        };
        for _ in 0..100 {
            let current = connection.request(RequestEnvelope::new(ClientRequest::GetTask {
                project_id,
                task_id,
            }));
            if let Ok(ServerResponse::Task(snapshot)) = current.result
                && matches!(
                    snapshot.status,
                    TaskStatus::Completed | TaskStatus::Failed | TaskStatus::Cancelled
                )
            {
                let evidence =
                    connection.request(RequestEnvelope::new(ClientRequest::GetTaskEvidence {
                        project_id,
                        task_id,
                    }));
                assert!(matches!(
                    evidence.result,
                    Ok(ServerResponse::TaskEvidence { .. })
                ));
                fs::remove_dir_all(root).unwrap();
                return;
            }
            thread::sleep(Duration::from_millis(10));
        }
        panic!("task evidence fixture did not finish");
    }

    /// Serves an event stream that keeps a completion open until the client
    /// gives up, so a run can be controlled while the model is still working.
    fn slow_model_endpoint() -> (String, std::sync::mpsc::Receiver<()>) {
        use std::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let (sender, receiver) = std::sync::mpsc::channel();
        thread::spawn(move || {
            use std::io::{Read, Write};

            let Ok((mut stream, _)) = listener.accept() else {
                return;
            };
            let mut request = [0_u8; 8192];
            let _ = stream.read(&mut request);
            let _ = stream.write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n",
            );
            let _ = stream
                .write_all(b"data: {\"choices\":[{\"delta\":{\"content\":\"thinking\"}}]}\n\n");
            let _ = stream.flush();
            let _ = sender.send(());
            // Keep the completion open; the run must be stoppable anyway.
            for _ in 0..600 {
                if stream
                    .write_all(b"data: {\"choices\":[{\"delta\":{\"content\":\".\"}}]}\n\n")
                    .is_err()
                {
                    return;
                }
                let _ = stream.flush();
                thread::sleep(Duration::from_millis(20));
            }
        });
        (format!("http://{address}/v1/chat/completions"), receiver)
    }

    #[test]
    fn a_running_model_call_can_be_interrupted_without_blocking_the_request() {
        let root = workspace();
        let (endpoint, started) = slow_model_endpoint();
        let backend =
            InProcessBackend::with_openai_compatible(endpoint, "key", ModelId::new("slow/model"));
        let connection = backend.connect();
        negotiate_m3(&connection);
        let project_id = ProjectId::new();
        let session = connection.request(RequestEnvelope::new(ClientRequest::CreateAgentSession {
            project_id,
            name: "interruptible run".to_owned(),
        }));
        let session_id = match session.result.unwrap() {
            ServerResponse::AgentSessionCreated(snapshot) => snapshot.id,
            response => panic!("unexpected response: {response:?}"),
        };
        let started_run = connection.request(RequestEnvelope::new(ClientRequest::StartAgentRun {
            session_id,
            task: "stream for a long time".to_owned(),
            model: ModelId::new("slow/model"),
            workspace_root: root.display().to_string(),
            system_instructions: None,
            repository_instructions: None,
        }));
        let run_id = match started_run.result.unwrap() {
            ServerResponse::AgentRunStarted(snapshot) => snapshot.id,
            response => panic!("unexpected response: {response:?}"),
        };
        started
            .recv_timeout(Duration::from_secs(10))
            .expect("model stream started");

        // A second connection controls the run while the first one's model call
        // is still open.
        let observer = backend.connect();
        negotiate_m3(&observer);
        // The delta is journaled while the completion is still open, so a second
        // client sees it before the run ends.
        let mut streamed = false;
        for _ in 0..1_000 {
            let events = observer.request(RequestEnvelope::new(ClientRequest::GetSessionEvents {
                session_id: Some(session_id),
                after_sequence: None,
            }));
            let ServerResponse::SessionEvents { events } = events.result.unwrap() else {
                panic!("unexpected events response");
            };
            if events.iter().any(|event| {
                matches!(
                    &event.event,
                    ServerEvent::Agent {
                        event: AgentEvent::AssistantMessageDelta { text, .. }
                    } if text == "thinking"
                )
            }) {
                streamed = true;
                break;
            }
            thread::sleep(Duration::from_millis(5));
        }
        assert!(
            streamed,
            "an assistant delta was not journaled mid-completion"
        );

        let before = Instant::now();
        let interrupted =
            observer.request(RequestEnvelope::new(ClientRequest::InterruptAgentRun {
                run_id,
            }));
        let elapsed = before.elapsed();
        let ServerResponse::AgentRun(snapshot) = interrupted.result.unwrap() else {
            panic!("unexpected interrupt response");
        };
        assert_eq!(snapshot.state, AgentRunState::Cancelled);
        assert!(
            elapsed < Duration::from_secs(5),
            "interrupt waited {elapsed:?} for the model call"
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_running_model_call_can_be_paused_and_resumed() {
        let root = workspace();
        let (endpoint, started) = slow_model_endpoint();
        let backend =
            InProcessBackend::with_openai_compatible(endpoint, "key", ModelId::new("slow/model"));
        let connection = backend.connect();
        negotiate_m3(&connection);
        let project_id = ProjectId::new();
        let session = connection.request(RequestEnvelope::new(ClientRequest::CreateAgentSession {
            project_id,
            name: "pausable run".to_owned(),
        }));
        let session_id = match session.result.unwrap() {
            ServerResponse::AgentSessionCreated(snapshot) => snapshot.id,
            response => panic!("unexpected response: {response:?}"),
        };
        let started_run = connection.request(RequestEnvelope::new(ClientRequest::StartAgentRun {
            session_id,
            task: "stream for a long time".to_owned(),
            model: ModelId::new("slow/model"),
            workspace_root: root.display().to_string(),
            system_instructions: None,
            repository_instructions: None,
        }));
        let run_id = match started_run.result.unwrap() {
            ServerResponse::AgentRunStarted(snapshot) => snapshot.id,
            response => panic!("unexpected response: {response:?}"),
        };
        started
            .recv_timeout(Duration::from_secs(10))
            .expect("model stream started");
        let before = Instant::now();
        let paused = connection.request(RequestEnvelope::new(ClientRequest::PauseAgentRun {
            run_id,
        }));
        let elapsed = before.elapsed();
        let ServerResponse::AgentRun(snapshot) = paused.result.unwrap() else {
            panic!("unexpected pause response");
        };
        assert_eq!(snapshot.state, AgentRunState::Paused);
        assert!(
            elapsed < Duration::from_secs(5),
            "pause waited {elapsed:?} for the model call"
        );
        let current =
            connection.request(RequestEnvelope::new(ClientRequest::GetAgentRun { run_id }));
        let ServerResponse::AgentRun(snapshot) = current.result.unwrap() else {
            panic!("unexpected run response");
        };
        assert_eq!(snapshot.state, AgentRunState::Paused);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn retryable_mutation_idempotency_survives_backend_restart() {
        let path =
            std::env::temp_dir().join(format!("loom-server-idempotency-{}.db", ProjectId::new()));
        let request_id = loom_core::RequestId::new();
        let request = RequestEnvelope::with_request_id(
            request_id,
            ClientRequest::CreateAgentSession {
                project_id: ProjectId::new(),
                name: "durable idempotency".to_owned(),
            },
        );
        let first = {
            let backend = InProcessBackend::new_persistent(&path).unwrap();
            let connection = backend.connect();
            negotiate(&connection);
            connection.request(request.clone())
        };
        let backend = InProcessBackend::new_persistent(&path).unwrap();
        let connection = backend.connect();
        negotiate(&connection);
        let second = connection.request(request);
        assert_eq!(first, second);
        assert!(matches!(
            first.result,
            Ok(ServerResponse::AgentSessionCreated(_))
        ));
        fs::remove_file(path).unwrap();
    }

    #[allow(dead_code)]
    fn _keep_tool_id_in_scope(_: ToolCallId) {}
}
