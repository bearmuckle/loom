use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
    sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError, Weak},
    thread,
    time::{Duration, Instant},
};

use loom_agent::{
    AgentEvent, AgentEventObserver, AgentRunSnapshot, AgentRunState, AgentRuntime,
    AgentRuntimeOptions, AgentRuntimeState, AgentTask, RunControl, RunProgress,
};
use loom_core::{
    AgentSessionId, AgentSessionSnapshot, AgentSessionState, ApprovalPolicy, Capability,
    CapabilitySet, ErrorCode, EventSequence, LoomError, ProtocolVersion, RepositoryId, Result,
    SessionEventRecord, Timestamp, WorkspaceId, WorkspaceRecord,
};
use loom_model::{ModelCapabilities, ModelDescriptor, ModelId, ProviderId};
use loom_persistence::{CURRENT_SCHEMA_VERSION, FilePersistence};
use loom_process::{TaskSupervisor, TerminalManager};
use loom_protocol::{
    AgentRunSnapshotProjection, AgentSessionSnapshotProjection, CURRENT_PROTOCOL_VERSION,
    ClientRequest, GitHubCopilotLoginStatus, NegotiationResult, RequestEnvelope, ResponseEnvelope,
    ServerEventEnvelope, ServerResponse, SessionFilesystemChange, SessionFilesystemFile,
    SessionFilesystemSnapshot, SessionRepository, WorkerNodeResources, WorkerNodeStatus,
    WorkspaceConfig, unsupported_version_error,
};
use loom_providers::{
    CredentialRef, FileCredentialStore, GITHUB_COPILOT_CREDENTIAL_REF, GitHubCopilotAuthenticator,
    ModelProvider, ProviderConfig, ProviderHealth, ProviderRegistry, UnavailableProvider,
    UsageLedger, deterministic_descriptor,
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

fn checked_session_relative_path(relative: &str) -> Result<PathBuf> {
    let path = Path::new(relative);
    if relative.trim().is_empty()
        || relative.contains('\\')
        || path.is_absolute()
        || path
            .components()
            .any(|component| !matches!(component, std::path::Component::Normal(_)))
    {
        return Err(LoomError::invalid_request(
            "session repository path must be a normalized relative path",
        ));
    }
    Ok(path.to_path_buf())
}

fn copy_filesystem_tree(source: &Path, destination: &Path) -> Result<()> {
    let entries = fs::read_dir(source).map_err(|error| {
        LoomError::new(
            ErrorCode::WorkspaceAccessDenied,
            format!("could not read source session filesystem: {error}"),
            false,
        )
    })?;
    for entry in entries {
        let entry = entry.map_err(|error| {
            LoomError::new(
                ErrorCode::WorkspaceAccessDenied,
                format!("could not inspect source session filesystem: {error}"),
                false,
            )
        })?;
        let source_path = entry.path();
        let destination_path = destination.join(entry.file_name());
        let file_type = entry.file_type().map_err(|error| {
            LoomError::new(
                ErrorCode::WorkspaceAccessDenied,
                format!("could not inspect session filesystem entry: {error}"),
                false,
            )
        })?;
        if file_type.is_dir() {
            fs::create_dir(&destination_path).map_err(|error| {
                LoomError::new(
                    ErrorCode::WorkspaceAccessDenied,
                    format!("could not create copied session directory: {error}"),
                    false,
                )
            })?;
            copy_filesystem_tree(&source_path, &destination_path)?;
        } else if file_type.is_file() {
            fs::copy(&source_path, &destination_path).map_err(|error| {
                LoomError::new(
                    ErrorCode::WorkspaceAccessDenied,
                    format!("could not copy session file: {error}"),
                    false,
                )
            })?;
        } else if file_type.is_symlink() {
            copy_session_symlink(&source_path, &destination_path)?;
        } else {
            return Err(LoomError::new(
                ErrorCode::WorkspaceAccessDenied,
                format!(
                    "cannot copy unsupported filesystem entry '{}'",
                    source_path.display()
                ),
                false,
            ));
        }
    }
    Ok(())
}

#[cfg(unix)]
fn copy_session_symlink(source: &Path, destination: &Path) -> Result<()> {
    let target = fs::read_link(source).map_err(|error| {
        LoomError::new(
            ErrorCode::WorkspaceAccessDenied,
            format!("could not read session symlink: {error}"),
            false,
        )
    })?;
    std::os::unix::fs::symlink(target, destination).map_err(|error| {
        LoomError::new(
            ErrorCode::WorkspaceAccessDenied,
            format!("could not copy session symlink: {error}"),
            false,
        )
    })
}

#[cfg(windows)]
fn copy_session_symlink(source: &Path, destination: &Path) -> Result<()> {
    let target = fs::read_link(source).map_err(|error| {
        LoomError::new(
            ErrorCode::WorkspaceAccessDenied,
            format!("could not read session symlink: {error}"),
            false,
        )
    })?;
    let target_is_dir = fs::metadata(source).is_ok_and(|metadata| metadata.is_dir());
    let result = if target_is_dir {
        std::os::windows::fs::symlink_dir(target, destination)
    } else {
        std::os::windows::fs::symlink_file(target, destination)
    };
    result.map_err(|error| {
        LoomError::new(
            ErrorCode::WorkspaceAccessDenied,
            format!("could not copy session symlink: {error}"),
            false,
        )
    })
}

fn checked_session_path(root: &Path, relative: &str) -> Result<PathBuf> {
    let relative = checked_session_relative_path(relative)?;
    let root = fs::canonicalize(root).map_err(|error| {
        LoomError::new(
            ErrorCode::WorkspaceAccessDenied,
            format!("could not resolve session filesystem root: {error}"),
            false,
        )
    })?;
    let path = root.join(relative);
    let canonical = fs::canonicalize(&path).map_err(|error| {
        LoomError::new(
            ErrorCode::WorkspaceAccessDenied,
            format!("could not resolve session repository path: {error}"),
            false,
        )
    })?;
    if !canonical.starts_with(&root) || canonical == root {
        return Err(LoomError::new(
            ErrorCode::WorkspaceAccessDenied,
            "session repository path escapes its filesystem root",
            false,
        ));
    }
    Ok(canonical)
}

fn repository_display_name(source: &str) -> Result<String> {
    if Path::new(source).is_absolute() {
        let repository = GitService::open(source)?;
        return Ok(repository
            .root()
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| "local repository".to_owned()));
    }
    let parsed = url::Url::parse(source).map_err(|_| {
        LoomError::invalid_request("repository source must be an absolute path or URL")
    })?;
    if !matches!(parsed.scheme(), "https" | "ssh")
        || parsed.host_str().is_none_or(str::is_empty)
        || !parsed.password().unwrap_or_default().is_empty()
        || parsed.query_pairs().any(|(key, _)| {
            matches!(
                key.to_ascii_lowercase().as_str(),
                "token" | "access_token" | "auth" | "password" | "api_key" | "secret"
            )
        })
    {
        return Err(LoomError::invalid_request(
            "repository URLs must use HTTPS or SSH and must not embed credentials",
        ));
    }
    let name = parsed
        .path_segments()
        .and_then(|mut segments| segments.next_back())
        .filter(|name| !name.is_empty())
        .unwrap_or(parsed.host_str().unwrap_or("repository"))
        .trim_end_matches(".git");
    Ok(name.to_owned())
}

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
    workspace_records: loom_session::WorkspaceManagerState,
    journal: EventJournal,
    runs: BTreeMap<loom_core::RunId, AgentRuntimeState>,
    session_filesystems: BTreeMap<AgentSessionId, PersistedSessionFilesystem>,
    session_policies: BTreeMap<AgentSessionId, ApprovalPolicy>,
    auto_approve_actions: BTreeMap<AgentSessionId, bool>,
    provider_configs: Vec<ProviderConfig>,
    provider_health: BTreeMap<ProviderId, ProviderHealth>,
    workspace_configs: BTreeMap<WorkspaceId, WorkspaceConfig>,
    provider_usage: UsageLedger,
    idempotency: BTreeMap<loom_core::RequestId, IdempotencyRecord>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct PersistedSessionFilesystem {
    filesystem: loom_workspace::WorkspaceStateSnapshot,
    repositories: BTreeMap<RepositoryId, SessionRepository>,
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
    system_instructions: Option<String>,
    repository_instructions: Option<String>,
    options: AgentRuntimeOptions,
}

struct GitHubCopilotLoginRecord {
    status: GitHubCopilotLoginStatus,
    expires_at: Instant,
}

pub struct InProcessBackend {
    node_id: String,
    node_name: String,
    sessions: Mutex<SessionManager>,
    workspace_records: Mutex<loom_session::WorkspaceManager>,
    runs: Mutex<BTreeMap<loom_core::RunId, Arc<RunHandle>>>,
    journal: Mutex<EventJournal>,
    session_filesystems: Mutex<BTreeMap<AgentSessionId, Workspace>>,
    session_repositories:
        Mutex<BTreeMap<AgentSessionId, BTreeMap<RepositoryId, SessionRepository>>>,
    session_vcs: Mutex<BTreeMap<(AgentSessionId, RepositoryId), GitService>>,
    session_task_supervisors: Mutex<BTreeMap<AgentSessionId, TaskSupervisor>>,
    session_policies: Mutex<BTreeMap<AgentSessionId, ApprovalPolicy>>,
    auto_approve_actions: Mutex<BTreeMap<AgentSessionId, bool>>,
    workspace_configs: Mutex<BTreeMap<WorkspaceId, WorkspaceConfig>>,
    session_terminals: Mutex<BTreeMap<loom_core::TerminalId, AgentSessionId>>,
    terminals: TerminalManager,
    resource_monitor: Mutex<ResourceMonitor>,
    supported_capabilities: CapabilitySet,
    models: Vec<ModelDescriptor>,
    providers: ProviderRegistry,
    github_copilot_logins: Mutex<BTreeMap<String, GitHubCopilotLoginRecord>>,
    persistence: Option<FilePersistence>,
    session_root_base: PathBuf,
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

fn openai_compatible_descriptor(model: ModelId) -> ModelDescriptor {
    ModelDescriptor {
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
    }
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
        let descriptor = openai_compatible_descriptor(model.into());
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
        let descriptor = openai_compatible_descriptor(model.into());
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
        let descriptor = openai_compatible_descriptor(model.into());
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
        let session_root_base = persistence.as_ref().map_or_else(
            || {
                std::env::temp_dir().join(format!(
                    "loom-session-roots-{node_id}-{}",
                    WorkspaceId::new()
                ))
            },
            |persistence| persistence.path().with_extension("session-roots"),
        );
        let backend = Arc::new(Self {
            node_id,
            node_name,
            sessions: Mutex::new(SessionManager::default()),
            workspace_records: Mutex::new(loom_session::WorkspaceManager::default()),
            runs: Mutex::new(BTreeMap::new()),
            journal: Mutex::new(EventJournal::default()),
            session_filesystems: Mutex::new(BTreeMap::new()),
            session_repositories: Mutex::new(BTreeMap::new()),
            session_vcs: Mutex::new(BTreeMap::new()),
            session_task_supervisors: Mutex::new(BTreeMap::new()),
            session_policies: Mutex::new(BTreeMap::new()),
            auto_approve_actions: Mutex::new(BTreeMap::new()),
            workspace_configs: Mutex::new(BTreeMap::new()),
            session_terminals: Mutex::new(BTreeMap::new()),
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
                Capability::ConfigureProviders,
                Capability::ReadProviderHealth,
                Capability::ReadUsage,
                Capability::InspectContext,
                Capability::ReadWorkspaceConfig,
                Capability::OpenSessionTerminal,
                Capability::ControlSessionTerminal,
                Capability::ReadSessionTask,
                Capability::StartSessionTask,
                Capability::ControlSessionTask,
                Capability::ConfigureApprovalPolicy,
                Capability::ManageCheckpoints,
                Capability::ReadVcsStatus,
                Capability::ReadVcsDiff,
                Capability::ReadSessionTaskEvidence,
                Capability::ReadWorkerNodeStatus,
                Capability::JsonProtocol,
                Capability::ManageWorkspaces,
                Capability::ManageSessionRepositories,
                Capability::ReadSessionFilesystem,
                Capability::WriteSessionFilesystem,
            ]),
            models,
            providers,
            github_copilot_logins: Mutex::new(BTreeMap::new()),
            persistence,
            session_root_base,
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

    fn workspace_records(&self) -> Result<MutexGuard<'_, loom_session::WorkspaceManager>> {
        self.workspace_records.lock().map_err(|_| {
            LoomError::new(
                ErrorCode::Internal,
                "workspace record manager lock was poisoned",
                true,
            )
        })
    }

    fn session_filesystems(&self) -> Result<MutexGuard<'_, BTreeMap<AgentSessionId, Workspace>>> {
        self.session_filesystems.lock().map_err(|_| {
            LoomError::new(
                ErrorCode::Internal,
                "session filesystem manager lock was poisoned",
                true,
            )
        })
    }

    fn session_repositories(
        &self,
    ) -> Result<MutexGuard<'_, BTreeMap<AgentSessionId, BTreeMap<RepositoryId, SessionRepository>>>>
    {
        self.session_repositories.lock().map_err(|_| {
            LoomError::new(
                ErrorCode::Internal,
                "session repository manager lock was poisoned",
                true,
            )
        })
    }

    fn session_vcs(
        &self,
    ) -> Result<MutexGuard<'_, BTreeMap<(AgentSessionId, RepositoryId), GitService>>> {
        self.session_vcs.lock().map_err(|_| {
            LoomError::new(
                ErrorCode::Internal,
                "session Git service manager lock was poisoned",
                true,
            )
        })
    }

    fn session_task_supervisors(
        &self,
    ) -> Result<MutexGuard<'_, BTreeMap<AgentSessionId, TaskSupervisor>>> {
        self.session_task_supervisors.lock().map_err(|_| {
            LoomError::new(
                ErrorCode::Internal,
                "session task supervisor lock was poisoned",
                true,
            )
        })
    }

    fn session_terminals(
        &self,
    ) -> Result<MutexGuard<'_, BTreeMap<loom_core::TerminalId, AgentSessionId>>> {
        self.session_terminals.lock().map_err(|_| {
            LoomError::new(
                ErrorCode::Internal,
                "session terminal lock was poisoned",
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

    fn session_policies(&self) -> Result<MutexGuard<'_, BTreeMap<AgentSessionId, ApprovalPolicy>>> {
        self.session_policies.lock().map_err(|_| {
            LoomError::new(
                ErrorCode::Internal,
                "session approval policy lock was poisoned",
                true,
            )
        })
    }

    fn auto_approve_actions(&self) -> Result<MutexGuard<'_, BTreeMap<AgentSessionId, bool>>> {
        self.auto_approve_actions.lock().map_err(|_| {
            LoomError::new(
                ErrorCode::Internal,
                "session approval settings lock was poisoned",
                true,
            )
        })
    }

    fn workspace_configs(&self) -> Result<MutexGuard<'_, BTreeMap<WorkspaceId, WorkspaceConfig>>> {
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

    fn set_workspace_config(
        &self,
        workspace_id: WorkspaceId,
        config: WorkspaceConfig,
    ) -> Result<()> {
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
            .get(&workspace_id)
            .map(|config| config.revision);
        if current_revision.is_some_and(|revision| revision > config.revision) {
            return Ok(());
        }
        let previous = self.workspace_configs()?.insert(workspace_id, config);
        if let Err(error) = self.persist_state() {
            let mut configs = self.workspace_configs()?;
            if let Some(previous) = previous {
                configs.insert(workspace_id, previous);
            } else {
                configs.remove(&workspace_id);
            }
            return Err(error);
        }
        Ok(())
    }

    fn create_session_filesystem(
        &self,
        workspace_id: WorkspaceId,
        session_id: AgentSessionId,
    ) -> Result<Workspace> {
        let root = self
            .session_root_base
            .join(workspace_id.to_string())
            .join(session_id.to_string())
            .join("fs");
        fs::create_dir_all(&root).map_err(|error| {
            LoomError::new(
                ErrorCode::WorkspaceAccessDenied,
                format!("could not create session filesystem root: {error}"),
                false,
            )
        })?;
        Workspace::open(session_id, root)
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
        let session_policies = persistence
            .load_section("session_approval_policies", CURRENT_SCHEMA_VERSION)?
            .map(from_json)
            .transpose()?;
        let state = PersistedBackendState {
            sessions,
            workspace_records: persistence
                .load_section("workspace_records", CURRENT_SCHEMA_VERSION)?
                .map(from_json)
                .transpose()?
                .unwrap_or_default(),
            journal: from_json(required("journal")?)?,
            runs: from_json(required("runs")?)?,
            session_filesystems: persistence
                .load_section("session_filesystems", CURRENT_SCHEMA_VERSION)?
                .map(from_json)
                .transpose()?
                .unwrap_or_default(),
            session_policies: session_policies.unwrap_or_default(),
            auto_approve_actions: persistence
                .load_section("auto_approve_actions", CURRENT_SCHEMA_VERSION)?
                .map(from_json)
                .transpose()?
                .unwrap_or_default(),
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
            let mut target = self.session_policies()?;
            *target = state.session_policies;
        }
        {
            let mut target = self.auto_approve_actions()?;
            *target = state.auto_approve_actions;
        }
        self.providers.restore_configs(state.provider_configs)?;
        self.providers.restore_health(state.provider_health)?;
        self.providers.restore_usage(state.provider_usage)?;
        *self.workspace_configs()? = state.workspace_configs;

        *self.workspace_records()? =
            loom_session::WorkspaceManager::from_state(state.workspace_records)?;

        let mut session_filesystems = self.session_filesystems()?;
        let mut session_repositories = self.session_repositories()?;
        for (session_id, persisted) in state.session_filesystems {
            let session = self.sessions()?.get(session_id)?;
            let expected_root = self
                .session_root_base
                .join(session.workspace_id.to_string())
                .join(session_id.to_string())
                .join("fs");
            let canonical_expected_root = fs::canonicalize(&expected_root).map_err(|error| {
                LoomError::new(
                    ErrorCode::RecoveryRequired,
                    format!("session filesystem root for {session_id} is unavailable: {error}"),
                    true,
                )
            })?;
            if Path::new(&persisted.filesystem.root) != canonical_expected_root {
                return Err(LoomError::new(
                    ErrorCode::MalformedPayload,
                    format!("persisted filesystem root for session {session_id} is invalid"),
                    false,
                ));
            }
            let filesystem = Workspace::open(session_id, &canonical_expected_root)?;
            filesystem.restore_state(persisted.filesystem)?;
            for (repository_id, repository) in &persisted.repositories {
                if *repository_id != repository.id {
                    return Err(LoomError::new(
                        ErrorCode::MalformedPayload,
                        format!("repository key does not match repository {}", repository.id),
                        false,
                    ));
                }
                let path = checked_session_path(filesystem.root(), &repository.path)?;
                let service = GitService::open(path)?;
                self.session_vcs()?
                    .insert((session_id, *repository_id), service);
            }
            session_filesystems.insert(session_id, filesystem);
            session_repositories.insert(session_id, persisted.repositories);
        }
        drop(session_repositories);
        drop(session_filesystems);

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
                .session_filesystems()?
                .get(&session.id)
                .cloned()
                .ok_or_else(|| {
                    LoomError::new(
                        ErrorCode::RecoveryRequired,
                        format!("filesystem for persisted run {run_id} is unavailable"),
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
        let session_filesystems = self
            .session_filesystems()?
            .iter()
            .map(|(session_id, filesystem)| {
                Ok((
                    *session_id,
                    PersistedSessionFilesystem {
                        filesystem: filesystem.export_state()?,
                        repositories: self
                            .session_repositories()?
                            .get(session_id)
                            .cloned()
                            .unwrap_or_default(),
                    },
                ))
            })
            .collect::<Result<BTreeMap<_, _>>>()?;
        persistence.save_sections(
            CURRENT_SCHEMA_VERSION,
            &[
                ("sessions", json_value(self.sessions()?.export_state())?),
                (
                    "workspace_records",
                    json_value(self.workspace_records()?.export_state())?,
                ),
                ("journal", json_value(self.journal()?.clone())?),
                ("runs", json_value(runs)?),
                ("session_filesystems", json_value(session_filesystems)?),
                (
                    "session_approval_policies",
                    json_value(self.session_policies()?.clone())?,
                ),
                (
                    "auto_approve_actions",
                    json_value(self.auto_approve_actions()?.clone())?,
                ),
                (
                    "workspace_configs",
                    json_value(self.workspace_configs()?.clone())?,
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
        let approval_policy = self.policy(session_id)?;
        let auto_approve_actions = self.auto_approve_actions(session_id)?;
        Ok(AgentSessionSnapshotProjection {
            session,
            active_run,
            latest_sequence,
            approval_policy,
            auto_approve_actions,
        })
    }

    fn create_workspace(&self, name: String) -> Result<WorkspaceRecord> {
        self.backend.workspace_records()?.create(name)
    }

    fn register_workspace(&self, workspace: WorkspaceRecord) -> Result<WorkspaceRecord> {
        self.backend.workspace_records()?.register(workspace)
    }

    fn rename_workspace(&self, workspace_id: WorkspaceId, name: String) -> Result<WorkspaceRecord> {
        self.backend.workspace_records()?.rename(workspace_id, name)
    }

    fn create_session_in_workspace(
        &self,
        workspace_id: WorkspaceId,
        name: String,
    ) -> Result<AgentSessionSnapshot> {
        self.backend.workspace_records()?.get(workspace_id)?;
        if name.trim().is_empty() {
            return Err(LoomError::invalid_request(
                "agent session name must not be empty",
            ));
        }
        let session_id = AgentSessionId::new();
        let filesystem = self
            .backend
            .create_session_filesystem(workspace_id, session_id)?;
        let (snapshot, record) =
            self.backend
                .sessions()?
                .create_in_workspace_with_id(workspace_id, session_id, name)?;
        self.backend
            .session_filesystems()?
            .insert(session_id, filesystem);
        self.backend
            .session_repositories()?
            .insert(session_id, BTreeMap::new());
        self.backend.journal()?.append_session(record);
        Ok(snapshot)
    }

    fn session_filesystem(&self, session_id: AgentSessionId) -> Result<Workspace> {
        self.backend
            .session_filesystems()?
            .get(&session_id)
            .cloned()
            .ok_or_else(|| {
                LoomError::new(
                    ErrorCode::RecoveryRequired,
                    format!("filesystem for session {session_id} is unavailable"),
                    true,
                )
            })
    }

    fn attach_session_repository(
        &self,
        session_id: AgentSessionId,
        source: String,
        relative_path: String,
        revision: Option<String>,
    ) -> Result<SessionRepository> {
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
            return Err(LoomError::invalid_state(
                "repositories cannot be attached while a session is active",
            ));
        }
        let source_name = repository_display_name(&source)?;
        let relative = checked_session_relative_path(&relative_path)?;
        let filesystem = self.session_filesystem(session_id)?;
        let root = filesystem.root();
        let destination = root.join(&relative);
        if destination.exists() {
            return Err(LoomError::conflict(format!(
                "session path '{}' already exists",
                relative.display()
            )));
        }
        let parent = destination.parent().ok_or_else(|| {
            LoomError::invalid_request("repository checkout path must have a parent")
        })?;
        fs::create_dir_all(parent).map_err(|error| {
            LoomError::new(
                ErrorCode::WorkspaceAccessDenied,
                format!("could not create repository checkout parent: {error}"),
                false,
            )
        })?;
        let canonical_root = fs::canonicalize(root).map_err(|error| {
            LoomError::new(
                ErrorCode::WorkspaceAccessDenied,
                format!("could not resolve session filesystem root: {error}"),
                false,
            )
        })?;
        let canonical_parent = fs::canonicalize(parent).map_err(|error| {
            LoomError::new(
                ErrorCode::WorkspaceAccessDenied,
                format!("could not resolve repository checkout parent: {error}"),
                false,
            )
        })?;
        if !canonical_parent.starts_with(&canonical_root) {
            return Err(LoomError::new(
                ErrorCode::WorkspaceAccessDenied,
                "repository checkout path escapes its session filesystem root",
                false,
            ));
        }
        let repository_id = RepositoryId::new();
        let temporary = parent.join(format!(".loom-clone-{repository_id}"));
        if temporary.exists() {
            return Err(LoomError::conflict(
                "temporary repository checkout path already exists",
            ));
        }
        let cloned = match GitService::clone_from(&source, &temporary, revision.as_deref()) {
            Ok(cloned) => cloned,
            Err(error) => {
                if temporary.exists() {
                    fs::remove_dir_all(&temporary).map_err(|cleanup_error| {
                        LoomError::new(
                            ErrorCode::ToolExecution,
                            format!(
                                "repository clone failed and temporary checkout cleanup failed: {cleanup_error}"
                            ),
                            false,
                        )
                    })?;
                }
                return Err(error);
            }
        };
        drop(cloned);
        if let Err(error) = fs::rename(&temporary, &destination) {
            let cleanup = fs::remove_dir_all(&temporary);
            if let Err(cleanup_error) = cleanup {
                return Err(LoomError::new(
                    ErrorCode::ToolExecution,
                    format!(
                        "could not install cloned repository ({error}) or clean its temporary checkout ({cleanup_error})"
                    ),
                    false,
                ));
            }
            return Err(LoomError::new(
                ErrorCode::ToolExecution,
                format!("could not install cloned repository: {error}"),
                false,
            ));
        }
        let service = GitService::open(&destination)?;
        let repository = SessionRepository {
            id: repository_id,
            source: source_name,
            path: relative_path,
            revision: service.status()?.head,
            attached_at: Timestamp::now(),
        };
        self.backend
            .session_vcs()?
            .insert((session_id, repository_id), service);
        self.backend
            .session_repositories()?
            .entry(session_id)
            .or_default()
            .insert(repository_id, repository.clone());
        Ok(repository)
    }

    fn detach_session_repository(
        &self,
        session_id: AgentSessionId,
        repository_id: RepositoryId,
    ) -> Result<()> {
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
            return Err(LoomError::invalid_state(
                "repositories cannot be detached while a session is active",
            ));
        }
        let repository = self
            .backend
            .session_repositories()?
            .get(&session_id)
            .and_then(|repositories| repositories.get(&repository_id))
            .cloned()
            .ok_or_else(|| LoomError::not_found("session repository", repository_id))?;
        let filesystem = self.session_filesystem(session_id)?;
        let path = checked_session_path(filesystem.root(), &repository.path)?;
        fs::remove_dir_all(&path).map_err(|error| {
            LoomError::new(
                ErrorCode::ToolExecution,
                format!("could not remove detached repository checkout: {error}"),
                false,
            )
        })?;
        self.backend
            .session_repositories()?
            .entry(session_id)
            .or_default()
            .remove(&repository_id);
        self.backend
            .session_vcs()?
            .remove(&(session_id, repository_id));
        Ok(())
    }

    fn session_git(
        &self,
        session_id: AgentSessionId,
        repository_id: RepositoryId,
    ) -> Result<GitService> {
        if let Some(service) = self
            .backend
            .session_vcs()?
            .get(&(session_id, repository_id))
            .cloned()
        {
            return Ok(service);
        }
        let repository = self
            .backend
            .session_repositories()?
            .get(&session_id)
            .and_then(|repositories| repositories.get(&repository_id))
            .cloned()
            .ok_or_else(|| LoomError::not_found("session repository", repository_id))?;
        let filesystem = self.session_filesystem(session_id)?;
        let path = checked_session_path(filesystem.root(), &repository.path)?;
        let service = GitService::open(path)?;
        self.backend
            .session_vcs()?
            .insert((session_id, repository_id), service.clone());
        Ok(service)
    }

    fn session_task_supervisor(&self, session_id: AgentSessionId) -> Result<TaskSupervisor> {
        let filesystem = self.session_filesystem(session_id)?;
        let mut supervisors = self.backend.session_task_supervisors()?;
        if let Some(supervisor) = supervisors.get(&session_id) {
            return Ok(supervisor.clone());
        }
        let supervisor = TaskSupervisor::new(filesystem.root())?;
        supervisors.insert(session_id, supervisor.clone());
        Ok(supervisor)
    }

    fn check_terminal_session(
        &self,
        session_id: AgentSessionId,
        terminal_id: loom_core::TerminalId,
    ) -> Result<()> {
        let owner = self
            .backend
            .session_terminals()?
            .get(&terminal_id)
            .copied()
            .ok_or_else(|| LoomError::not_found("terminal", terminal_id))?;
        if owner != session_id {
            return Err(LoomError::new(
                ErrorCode::WorkspaceAccessDenied,
                "terminal does not belong to the requested session",
                false,
            ));
        }
        Ok(())
    }

    fn policy(&self, session_id: AgentSessionId) -> Result<ApprovalPolicy> {
        if let Some(policy) = self.backend.session_policies()?.get(&session_id).cloned() {
            return Ok(policy);
        }
        Ok(ApprovalPolicy::auto_approve())
    }

    fn auto_approve_actions(&self, session_id: AgentSessionId) -> Result<bool> {
        let settings = self.backend.auto_approve_actions()?;
        if let Some(auto_approve_actions) = settings.get(&session_id) {
            return Ok(*auto_approve_actions);
        }
        drop(settings);
        Ok(self.policy(session_id)? == ApprovalPolicy::auto_approve())
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
            ClientRequest::CreateWorkspace { name } => Ok(ServerResponse::WorkspaceCreated(
                self.create_workspace(name)?,
            )),
            ClientRequest::RegisterWorkspace { workspace } => Ok(ServerResponse::WorkspaceCreated(
                self.register_workspace(workspace)?,
            )),
            ClientRequest::ListWorkspaces => {
                let workspaces = self
                    .backend
                    .workspace_records()?
                    .list()
                    .into_iter()
                    .filter(|workspace| {
                        self.auth
                            .as_ref()
                            .is_none_or(|auth| auth.scope().allows_workspace(workspace.id))
                    })
                    .collect();
                Ok(ServerResponse::Workspaces { workspaces })
            }
            ClientRequest::RenameWorkspace { workspace_id, name } => Ok(
                ServerResponse::WorkspaceRenamed(self.rename_workspace(workspace_id, name)?),
            ),
            ClientRequest::ListWorkspaceSessions {
                workspace_id,
                include_archived,
            } => Ok(ServerResponse::AgentSessions {
                sessions: self
                    .backend
                    .sessions()?
                    .list_in_workspace(Some(workspace_id), include_archived)
                    .into_iter()
                    .filter(|session| {
                        self.auth
                            .as_ref()
                            .is_none_or(|auth| auth.scope().allows_session(session.id))
                    })
                    .collect(),
            }),
            ClientRequest::CreateAgentSessionInWorkspace { workspace_id, name } => {
                Ok(ServerResponse::AgentSessionCreated(
                    self.create_session_in_workspace(workspace_id, name)?,
                ))
            }
            ClientRequest::GetWorkspaceConfigForWorkspace { workspace_id } => {
                Ok(ServerResponse::WorkspaceConfig(
                    self.backend
                        .workspace_configs()?
                        .get(&workspace_id)
                        .cloned()
                        .unwrap_or_default(),
                ))
            }
            ClientRequest::SetWorkspaceConfigForWorkspace {
                workspace_id,
                config,
            } => {
                self.backend.set_workspace_config(workspace_id, config)?;
                Ok(ServerResponse::WorkspaceConfigUpdated)
            }
            ClientRequest::AttachSessionRepository {
                session_id,
                source,
                path,
                revision,
            } => Ok(ServerResponse::SessionRepositoryAttached(
                self.attach_session_repository(session_id, source, path, revision)?,
            )),
            ClientRequest::ListSessionRepositories { session_id } => {
                self.backend.sessions()?.get(session_id)?;
                Ok(ServerResponse::SessionRepositories {
                    repositories: self
                        .backend
                        .session_repositories()?
                        .get(&session_id)
                        .map(|repositories| repositories.values().cloned().collect())
                        .unwrap_or_default(),
                })
            }
            ClientRequest::DetachSessionRepository {
                session_id,
                repository_id,
            } => {
                self.detach_session_repository(session_id, repository_id)?;
                Ok(ServerResponse::SessionRepositoryDetached)
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
            ClientRequest::StartSessionAgentRun {
                session_id,
                task,
                model,
                system_instructions,
                repository_instructions,
            } => self.start_run_with_options(StartRunInput {
                session_id,
                task,
                model,
                system_instructions,
                repository_instructions,
                options: AgentRuntimeOptions::default(),
            }),
            ClientRequest::StartSessionAgentRunWithOptions {
                session_id,
                task,
                model,
                system_instructions,
                repository_instructions,
                limits,
                context,
            } => self.start_run_with_options(StartRunInput {
                session_id,
                task,
                model,
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
                let (session_id, checkpoint_id) = {
                    let handle = self.run_handle(run_id)?;
                    let session = self.backend.sessions()?.get(handle.session_id)?;
                    (
                        session.id,
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
                    self.session_filesystem(session_id)?
                        .checkpoint(checkpoint_id)?,
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
                let source = self.backend.sessions()?.get(session_id)?;
                if name.trim().is_empty() {
                    return Err(LoomError::invalid_request(
                        "forked agent session name must not be empty",
                    ));
                }
                let approval_policy = self.policy(session_id)?;
                let auto_approve_actions = self.auto_approve_actions(session_id)?;
                let source_filesystem = self.session_filesystem(session_id)?;
                let target_id = AgentSessionId::new();
                let target_root = self
                    .backend
                    .session_root_base
                    .join(source.workspace_id.to_string())
                    .join(target_id.to_string())
                    .join("fs");
                fs::create_dir_all(&target_root).map_err(|error| {
                    LoomError::new(
                        ErrorCode::WorkspaceAccessDenied,
                        format!("could not create forked session filesystem: {error}"),
                        false,
                    )
                })?;
                if let Err(error) = copy_filesystem_tree(source_filesystem.root(), &target_root) {
                    let _ = fs::remove_dir_all(&target_root);
                    return Err(error);
                }
                let target_filesystem = Workspace::open(target_id, &target_root)?;
                let mut target_repositories = BTreeMap::new();
                let source_repositories = self
                    .backend
                    .session_repositories()?
                    .get(&session_id)
                    .cloned()
                    .unwrap_or_default();
                let mut target_vcs = BTreeMap::new();
                for repository in source_repositories.values() {
                    let repository_path = checked_session_path(&target_root, &repository.path)?;
                    let service = GitService::open(&repository_path)?;
                    let id = RepositoryId::new();
                    let forked_repository = SessionRepository {
                        id,
                        source: repository.source.clone(),
                        path: repository.path.clone(),
                        revision: service.status()?.head,
                        attached_at: Timestamp::now(),
                    };
                    target_vcs.insert((target_id, id), service);
                    target_repositories.insert(id, forked_repository);
                }
                let (snapshot, record) = match self
                    .backend
                    .sessions()?
                    .fork_with_id(session_id, name, target_id)
                {
                    Ok(fork) => fork,
                    Err(error) => {
                        let _ = fs::remove_dir_all(&target_root);
                        return Err(error);
                    }
                };
                self.backend
                    .session_policies()?
                    .insert(target_id, approval_policy);
                self.backend
                    .auto_approve_actions()?
                    .insert(target_id, auto_approve_actions);
                self.backend
                    .session_filesystems()?
                    .insert(target_id, target_filesystem);
                self.backend
                    .session_repositories()?
                    .insert(target_id, target_repositories);
                self.backend.session_vcs()?.extend(target_vcs);
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
            ClientRequest::ConfigureGitHubCopilot { access_token } => {
                self.backend
                    .providers
                    .configure_github_copilot(access_token)?;
                Ok(ServerResponse::ProviderConfigured)
            }
            ClientRequest::StartGitHubCopilotLogin => self.start_github_copilot_login(),
            ClientRequest::GetGitHubCopilotLoginStatus { login_id } => {
                self.github_copilot_login_status(&login_id)
            }
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
            ClientRequest::GetSessionFilesystemSnapshot { session_id } => {
                let mut snapshot = self.session_filesystem(session_id)?.snapshot()?;
                snapshot.root = ".".to_owned();
                Ok(ServerResponse::SessionFilesystemSnapshot(
                    SessionFilesystemSnapshot {
                        session_id,
                        root: snapshot.root,
                        captured_at: snapshot.captured_at,
                        entries: snapshot.entries,
                    },
                ))
            }
            ClientRequest::GetSessionFilesystemChanges {
                session_id,
                after_sequence,
            } => {
                let mut changes = self
                    .session_filesystem(session_id)?
                    .changes_since(after_sequence)?;
                let truncated = changes.len() > MAX_REVIEW_CHANGES;
                if truncated {
                    changes = changes.split_off(changes.len() - MAX_REVIEW_CHANGES);
                }
                Ok(ServerResponse::SessionFilesystemChanges {
                    changes: changes
                        .into_iter()
                        .map(|change| SessionFilesystemChange {
                            sequence: change.sequence,
                            session_id,
                            path: change.path,
                            kind: change.kind,
                            revision: change.revision,
                        })
                        .collect(),
                    truncated,
                })
            }
            ClientRequest::ReadSessionFile { session_id, path } => {
                let mut file = self.session_filesystem(session_id)?.read_file(&path)?;
                file.content = bounded_review_text(&file.content, MAX_REVIEW_FILE_BYTES);
                Ok(ServerResponse::SessionFilesystemFile(
                    SessionFilesystemFile {
                        session_id,
                        path: file.path,
                        content: file.content,
                        revision: file.revision,
                    },
                ))
            }
            ClientRequest::ApplySessionFilesystemEdit { session_id, edit } => {
                Ok(ServerResponse::WorkspaceEditApplied(
                    self.session_filesystem(session_id)?.apply_user_edit(edit)?,
                ))
            }
            ClientRequest::TakeSessionFilesystemControl {
                session_id,
                control,
            } => {
                self.session_filesystem(session_id)?.take_control(control)?;
                Ok(ServerResponse::WorkspaceControl(control))
            }
            ClientRequest::CreateSessionCheckpoint { session_id, label } => {
                Ok(ServerResponse::CheckpointCreated(
                    self.session_filesystem(session_id)?
                        .create_checkpoint(label)?,
                ))
            }
            ClientRequest::RevertSessionCheckpoint {
                session_id,
                checkpoint_id,
            } => Ok(ServerResponse::CheckpointReverted(
                self.session_filesystem(session_id)?
                    .revert_checkpoint(checkpoint_id)?,
            )),
            ClientRequest::UndoSessionEdit { session_id } => Ok(ServerResponse::WorkspaceUndo(
                self.session_filesystem(session_id)?
                    .undo_last_agent_edit()?,
            )),
            ClientRequest::GetSessionContextFiles { session_id } => {
                Ok(ServerResponse::ContextFiles {
                    files: self.session_filesystem(session_id)?.context_files()?,
                })
            }
            ClientRequest::GetSessionVcsStatus {
                session_id,
                repository_id,
            } => Ok(ServerResponse::VcsStatus(
                self.session_git(session_id, repository_id)?.status()?,
            )),
            ClientRequest::GetSessionVcsDiff {
                session_id,
                repository_id,
                path,
                staged,
            } => {
                let mut diff = self
                    .session_git(session_id, repository_id)?
                    .diff(path.as_deref(), staged)?;
                diff.patch = bounded_review_text(&diff.patch, MAX_REVIEW_DIFF_BYTES);
                Ok(ServerResponse::VcsDiff(diff))
            }
            ClientRequest::GetSessionVcsBranches {
                session_id,
                repository_id,
            } => Ok(ServerResponse::VcsBranches {
                branches: self.session_git(session_id, repository_id)?.branches()?,
            }),
            ClientRequest::GetSessionVcsConflicts {
                session_id,
                repository_id,
            } => Ok(ServerResponse::VcsConflicts {
                paths: self.session_git(session_id, repository_id)?.conflicts()?,
            }),
            ClientRequest::SetSessionApprovalPolicy {
                session_id,
                policy,
                auto_approve_actions,
            } => {
                self.backend.sessions()?.get(session_id)?;
                self.backend
                    .session_policies()?
                    .insert(session_id, policy.clone());
                if let Some(auto_approve_actions) = auto_approve_actions {
                    self.backend
                        .auto_approve_actions()?
                        .insert(session_id, auto_approve_actions);
                }
                Ok(ServerResponse::ApprovalPolicy(policy))
            }
            ClientRequest::OpenSessionTerminal {
                session_id,
                command,
                args,
                cwd,
            } => {
                let filesystem = self.session_filesystem(session_id)?;
                let cwd = filesystem.directory_path(cwd.as_deref().unwrap_or("."))?;
                let snapshot = self.backend.terminals.open(command, args, cwd)?;
                self.backend
                    .session_terminals()?
                    .insert(snapshot.id, session_id);
                Ok(ServerResponse::TerminalOpened(snapshot))
            }
            ClientRequest::WriteSessionTerminalInput {
                session_id,
                terminal_id,
                input,
            } => {
                self.check_terminal_session(session_id, terminal_id)?;
                self.backend.terminals.write_input(terminal_id, &input)?;
                Ok(ServerResponse::Terminal(
                    self.backend.terminals.get(terminal_id)?,
                ))
            }
            ClientRequest::ResizeSessionTerminal {
                session_id,
                terminal_id,
                rows,
                columns,
            } => {
                self.check_terminal_session(session_id, terminal_id)?;
                Ok(ServerResponse::Terminal(self.backend.terminals.resize(
                    terminal_id,
                    rows,
                    columns,
                )?))
            }
            ClientRequest::GetSessionTerminalEvents {
                session_id,
                terminal_id,
                after_sequence,
            } => {
                self.check_terminal_session(session_id, terminal_id)?;
                Ok(ServerResponse::TerminalEvents {
                    events: self
                        .backend
                        .terminals
                        .events_since(terminal_id, after_sequence)?,
                })
            }
            ClientRequest::CancelSessionTerminal {
                session_id,
                terminal_id,
            } => {
                self.check_terminal_session(session_id, terminal_id)?;
                Ok(ServerResponse::Terminal(
                    self.backend.terminals.cancel(terminal_id)?,
                ))
            }
            ClientRequest::StartSessionTask { session_id, spec } => Ok(
                ServerResponse::TaskStarted(self.session_task_supervisor(session_id)?.start(spec)?),
            ),
            ClientRequest::ListSessionTasks { session_id } => Ok(ServerResponse::Tasks {
                tasks: self.session_task_supervisor(session_id)?.list()?,
            }),
            ClientRequest::GetSessionTask {
                session_id,
                task_id,
            } => Ok(ServerResponse::Task(
                self.session_task_supervisor(session_id)?.get(task_id)?,
            )),
            ClientRequest::GetSessionTaskEvents {
                session_id,
                task_id,
                after_sequence,
            } => Ok(ServerResponse::TaskEvents {
                events: self
                    .session_task_supervisor(session_id)?
                    .events_since(task_id, after_sequence)?,
            }),
            ClientRequest::CancelSessionTask {
                session_id,
                task_id,
            } => Ok(ServerResponse::Task(
                self.session_task_supervisor(session_id)?.cancel(task_id)?,
            )),
            ClientRequest::GetSessionTaskEvidence {
                session_id,
                task_id,
            } => Ok(ServerResponse::TaskEvidence {
                evidence: self
                    .session_task_supervisor(session_id)?
                    .get(task_id)?
                    .evidence,
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
        let storage_root = self
            .backend
            .session_filesystems()?
            .values()
            .next()
            .map(|filesystem| filesystem.root().to_path_buf())
            .or_else(|| {
                self.backend
                    .session_root_base
                    .parent()
                    .map(Path::to_path_buf)
            })
            .unwrap_or_else(|| self.backend.session_root_base.clone());
        let (disk_total_bytes, disk_available_bytes) = Self::disk_resources(&storage_root);
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

    fn start_github_copilot_login(&self) -> Result<ServerResponse> {
        const MAX_PENDING_LOGINS: usize = 8;
        const COMPLETED_LOGIN_RETENTION: Duration = Duration::from_secs(300);

        let now = Instant::now();
        let login_id = uuid::Uuid::new_v4().to_string();
        {
            let mut logins = self
                .backend
                .github_copilot_logins
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            logins.retain(|_, login| login.expires_at + COMPLETED_LOGIN_RETENTION > now);
            if logins
                .values()
                .filter(|login| matches!(login.status, GitHubCopilotLoginStatus::Pending))
                .count()
                >= MAX_PENDING_LOGINS
            {
                return Err(LoomError::new(
                    ErrorCode::Conflict,
                    "too many GitHub Copilot sign-ins are already pending on this worker",
                    true,
                ));
            }
            logins.insert(
                login_id.clone(),
                GitHubCopilotLoginRecord {
                    status: GitHubCopilotLoginStatus::Pending,
                    expires_at: now + Duration::from_secs(3600),
                },
            );
        }
        let device = match GitHubCopilotAuthenticator::default().begin() {
            Ok(device) => device,
            Err(error) => {
                self.backend
                    .github_copilot_logins
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .remove(&login_id);
                return Err(error);
            }
        };
        if let Some(login) = self
            .backend
            .github_copilot_logins
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get_mut(&login_id)
        {
            login.expires_at = now + Duration::from_secs(device.expires_in.min(3600));
        }

        let backend = self.backend.clone();
        let device_for_poll = device.clone();
        let worker_login_id = login_id.clone();
        let spawn_result = thread::Builder::new()
            .name("github-copilot-login".to_owned())
            .spawn(move || {
                let status = match GitHubCopilotAuthenticator::default()
                    .poll(&device_for_poll)
                    .and_then(|token| {
                        backend.providers.configure_github_copilot(token)?;
                        backend.persist_state()
                    }) {
                    Ok(()) => GitHubCopilotLoginStatus::Configured,
                    Err(error) => GitHubCopilotLoginStatus::Failed {
                        message: error.message,
                    },
                };
                let mut logins = backend
                    .github_copilot_logins
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner);
                if let Some(login) = logins.get_mut(&worker_login_id)
                    && matches!(login.status, GitHubCopilotLoginStatus::Pending)
                {
                    login.status = status;
                }
            });
        if let Err(error) = spawn_result {
            self.backend
                .github_copilot_logins
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .remove(&login_id);
            return Err(LoomError::new(
                ErrorCode::Internal,
                format!("could not start GitHub Copilot sign-in: {error}"),
                false,
            ));
        }

        Ok(ServerResponse::GitHubCopilotLoginStarted {
            login_id,
            user_code: device.user_code,
            verification_uri: device.verification_uri,
            expires_in: device.expires_in,
            interval: device.interval,
        })
    }

    fn github_copilot_login_status(&self, login_id: &str) -> Result<ServerResponse> {
        let mut logins = self
            .backend
            .github_copilot_logins
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let login = logins
            .get_mut(login_id)
            .ok_or_else(|| LoomError::not_found("GitHub Copilot sign-in", login_id))?;
        if matches!(login.status, GitHubCopilotLoginStatus::Pending)
            && Instant::now() >= login.expires_at
        {
            login.status = GitHubCopilotLoginStatus::Failed {
                message: "GitHub device authorization expired".to_owned(),
            };
        }
        Ok(ServerResponse::GitHubCopilotLoginStatus {
            status: login.status.clone(),
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

        let mut workspace_id = None;
        let mut session_id = None;
        let mut run_id = None;
        match request {
            ClientRequest::CreateWorkspace { .. } => {
                if auth.scope().workspaces.is_some() || auth.scope().sessions.is_some() {
                    return Err(LoomError::new(
                        ErrorCode::AuthorizationDenied,
                        "workspace-scoped tokens cannot create workspaces",
                        false,
                    ));
                }
            }
            ClientRequest::RegisterWorkspace { workspace } => {
                if auth.scope().sessions.is_some() {
                    return Err(LoomError::new(
                        ErrorCode::AuthorizationDenied,
                        "session-scoped tokens cannot register workspaces",
                        false,
                    ));
                }
                workspace_id = Some(workspace.id);
            }
            ClientRequest::CreateAgentSessionInWorkspace {
                workspace_id: requested_workspace,
                ..
            } => {
                if auth.scope().sessions.is_some() {
                    return Err(LoomError::new(
                        ErrorCode::AuthorizationDenied,
                        "session-scoped tokens cannot create sessions",
                        false,
                    ));
                }
                workspace_id = Some(*requested_workspace);
            }
            ClientRequest::RenameWorkspace {
                workspace_id: requested_workspace,
                ..
            }
            | ClientRequest::SetWorkspaceConfigForWorkspace {
                workspace_id: requested_workspace,
                ..
            } => {
                if auth.scope().sessions.is_some() {
                    return Err(LoomError::new(
                        ErrorCode::AuthorizationDenied,
                        "session-scoped tokens cannot modify workspace configuration",
                        false,
                    ));
                }
                workspace_id = Some(*requested_workspace);
            }
            ClientRequest::ListWorkspaceSessions {
                workspace_id: requested_workspace,
                ..
            }
            | ClientRequest::GetWorkspaceConfigForWorkspace {
                workspace_id: requested_workspace,
            } => workspace_id = Some(*requested_workspace),
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
            ClientRequest::StartSessionAgentRun {
                session_id: requested_session,
                ..
            }
            | ClientRequest::StartSessionAgentRunWithOptions {
                session_id: requested_session,
                ..
            }
            | ClientRequest::AttachSessionRepository {
                session_id: requested_session,
                ..
            }
            | ClientRequest::ListSessionRepositories {
                session_id: requested_session,
            }
            | ClientRequest::DetachSessionRepository {
                session_id: requested_session,
                ..
            }
            | ClientRequest::GetSessionFilesystemSnapshot {
                session_id: requested_session,
            }
            | ClientRequest::GetSessionFilesystemChanges {
                session_id: requested_session,
                ..
            }
            | ClientRequest::ReadSessionFile {
                session_id: requested_session,
                ..
            }
            | ClientRequest::ApplySessionFilesystemEdit {
                session_id: requested_session,
                ..
            }
            | ClientRequest::TakeSessionFilesystemControl {
                session_id: requested_session,
                ..
            }
            | ClientRequest::CreateSessionCheckpoint {
                session_id: requested_session,
                ..
            }
            | ClientRequest::RevertSessionCheckpoint {
                session_id: requested_session,
                ..
            }
            | ClientRequest::UndoSessionEdit {
                session_id: requested_session,
            }
            | ClientRequest::GetSessionContextFiles {
                session_id: requested_session,
            }
            | ClientRequest::GetSessionVcsStatus {
                session_id: requested_session,
                ..
            }
            | ClientRequest::GetSessionVcsDiff {
                session_id: requested_session,
                ..
            }
            | ClientRequest::GetSessionVcsBranches {
                session_id: requested_session,
                ..
            }
            | ClientRequest::GetSessionVcsConflicts {
                session_id: requested_session,
                ..
            }
            | ClientRequest::OpenSessionTerminal {
                session_id: requested_session,
                ..
            }
            | ClientRequest::WriteSessionTerminalInput {
                session_id: requested_session,
                ..
            }
            | ClientRequest::ResizeSessionTerminal {
                session_id: requested_session,
                ..
            }
            | ClientRequest::GetSessionTerminalEvents {
                session_id: requested_session,
                ..
            }
            | ClientRequest::CancelSessionTerminal {
                session_id: requested_session,
                ..
            }
            | ClientRequest::StartSessionTask {
                session_id: requested_session,
                ..
            }
            | ClientRequest::ListSessionTasks {
                session_id: requested_session,
            }
            | ClientRequest::GetSessionTask {
                session_id: requested_session,
                ..
            }
            | ClientRequest::GetSessionTaskEvents {
                session_id: requested_session,
                ..
            }
            | ClientRequest::CancelSessionTask {
                session_id: requested_session,
                ..
            }
            | ClientRequest::GetSessionTaskEvidence {
                session_id: requested_session,
                ..
            }
            | ClientRequest::SetSessionApprovalPolicy {
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
            ClientRequest::Negotiate { .. }
            | ClientRequest::DiscoverCapabilities
            | ClientRequest::ListWorkspaces
            | ClientRequest::ListModels
            | ClientRequest::GetWorkerNodeStatus
            | ClientRequest::ListProviders
            | ClientRequest::StartGitHubCopilotLogin
            | ClientRequest::GetGitHubCopilotLoginStatus { .. }
            | ClientRequest::DiscoverProviderModels { .. }
            | ClientRequest::GetProviderHealth { .. } => {}
            ClientRequest::ConfigureGitHubCopilot { .. } => {}
        }

        if let ClientRequest::AttachSessionRepository { source, .. } = request
            && Path::new(source).is_absolute()
            && !auth.scope().allows_repository_source(Path::new(source))
        {
            return Err(LoomError::new(
                ErrorCode::WorkspaceAccessDenied,
                "token is not authorized to attach a repository from that local path",
                false,
            ));
        }

        if let Some(session_id) = session_id {
            if !auth.scope().allows_session(session_id) {
                return Err(unauthorized_session(session_id));
            }
            let session = self.backend.sessions()?.get(session_id)?;
            if !auth.scope().allows_workspace(session.workspace_id) {
                return Err(LoomError::new(
                    ErrorCode::AuthorizationDenied,
                    "token is not authorized for the session's workspace",
                    false,
                ));
            }
            if workspace_id.is_some_and(|requested| requested != session.workspace_id) {
                return Err(LoomError::new(
                    ErrorCode::WorkspaceAccessDenied,
                    "session does not belong to the requested workspace",
                    false,
                ));
            }
            workspace_id = Some(session.workspace_id);
        } else if run_id.is_none()
            && workspace_id.is_none()
            && matches!(
                request,
                ClientRequest::GetSessionEvents {
                    session_id: None,
                    ..
                }
            )
            && (auth.scope().sessions.is_some() || auth.scope().workspaces.is_some())
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
            let session = self.backend.sessions()?.get(session_id)?;
            if !auth.scope().allows_workspace(session.workspace_id) {
                return Err(LoomError::new(
                    ErrorCode::AuthorizationDenied,
                    "token is not authorized for the run's workspace",
                    false,
                ));
            }
            workspace_id = Some(session.workspace_id);
        }

        if let Some(workspace_id) = workspace_id
            && !auth.scope().allows_workspace(workspace_id)
        {
            return Err(LoomError::new(
                ErrorCode::AuthorizationDenied,
                "token is not authorized for the requested workspace",
                false,
            ));
        }
        Ok(())
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
        let workspace = self.session_filesystem(session.id)?;
        if input.repository_instructions.is_none() {
            let instructions = workspace.instruction_text()?;
            if !instructions.trim().is_empty() {
                input.repository_instructions = Some(instructions);
            }
        }
        let checkpoint = workspace.create_checkpoint("before agent run")?;
        input.options.checkpoint_id = Some(checkpoint.id);
        let tools = ToolExecutor::new_with_workspace(workspace);
        let policy = self.policy(session.id)?;
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
        let session_id = self.backend.sessions()?.get(handle.session_id)?.id;
        if handle.state().options.checkpoint_id != Some(checkpoint_id) {
            return Err(LoomError::conflict(format!(
                "checkpoint {checkpoint_id} is not the checkpoint associated with run {run_id}"
            )));
        }
        self.session_filesystem(session_id)?
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
    use loom_core::{AgentSessionId, CapabilitySet, PolicyDecision, ToolCallId, WorkspaceId};
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
            capabilities: connection.backend.supported_capabilities.clone(),
        }));
        assert!(matches!(response.result, Ok(ServerResponse::Negotiated(_))));
    }

    fn negotiate_m2(connection: &InProcessConnection) {
        negotiate(connection);
    }

    fn negotiate_m3(connection: &InProcessConnection) {
        negotiate(connection);
    }

    fn negotiate_m5(connection: &InProcessConnection) {
        negotiate(connection);
    }

    #[test]
    fn worker_node_status_reports_capabilities_and_resources() {
        let backend = InProcessBackend::new();
        let connection = backend.connect();
        negotiate_m5(&connection);
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
            std::env::temp_dir().join(format!("loom-workspace-config-{}.db", WorkspaceId::new()));
        let workspace_id;
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
            let created =
                connection.request(RequestEnvelope::new(ClientRequest::CreateWorkspace {
                    name: "Config test".to_owned(),
                }));
            let Ok(ServerResponse::WorkspaceCreated(workspace)) = created.result else {
                panic!("expected workspace creation");
            };
            workspace_id = workspace.id;
            let response = connection.request(RequestEnvelope::new(
                ClientRequest::SetWorkspaceConfigForWorkspace {
                    workspace_id: workspace.id,
                    config: config.clone(),
                },
            ));
            assert!(matches!(
                response.result,
                Ok(ServerResponse::WorkspaceConfigUpdated)
            ));
        }

        {
            let backend = InProcessBackend::new_persistent(&path).unwrap();
            let connection = backend.connect();
            negotiate_m5(&connection);
            let response = connection.request(RequestEnvelope::new(
                ClientRequest::GetWorkspaceConfigForWorkspace { workspace_id },
            ));
            assert!(matches!(
                response.result,
                Ok(ServerResponse::WorkspaceConfig(saved)) if saved == config
            ));
            let response = connection.request(RequestEnvelope::new(
                ClientRequest::SetWorkspaceConfigForWorkspace {
                    workspace_id,
                    config: WorkspaceConfig {
                        revision: 0,
                        cpu_pulse_threshold_percent: 5,
                        worker_nodes: vec![WorkerNodeConfig {
                            url: "wss://stale.example/ws".to_owned(),
                        }],
                    },
                },
            ));
            assert!(matches!(
                response.result,
                Ok(ServerResponse::WorkspaceConfigUpdated)
            ));
            let response = connection.request(RequestEnvelope::new(
                ClientRequest::GetWorkspaceConfigForWorkspace { workspace_id },
            ));
            assert!(matches!(
                response.result,
                Ok(ServerResponse::WorkspaceConfig(saved)) if saved == config
            ));
            for url in [
                "wss://worker.example/ws?%61ccess_token=secret",
                "wss://user:secret@worker.example/ws",
            ] {
                let response = connection.request(RequestEnvelope::new(
                    ClientRequest::SetWorkspaceConfigForWorkspace {
                        workspace_id,
                        config: WorkspaceConfig {
                            revision: 2,
                            cpu_pulse_threshold_percent: 5,
                            worker_nodes: vec![WorkerNodeConfig {
                                url: url.to_owned(),
                            }],
                        },
                    },
                ));
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
        let root = std::env::temp_dir().join(format!("loom-server-{}", AgentSessionId::new()));
        fs::create_dir(&root).unwrap();
        root
    }

    fn git_repository() -> PathBuf {
        let root = workspace();
        let run = |arguments: &[&str]| {
            assert!(
                Command::new("git")
                    .args(["-C", root.to_str().unwrap()])
                    .args(arguments)
                    .status()
                    .unwrap()
                    .success()
            );
        };
        run(&["init", "-q"]);
        run(&["config", "user.name", "Loom Test"]);
        run(&["config", "user.email", "loom@example.test"]);
        fs::write(root.join("README.md"), "source\n").unwrap();
        run(&["add", "--", "README.md"]);
        run(&["commit", "-qm", "initial"]);
        root
    }

    #[test]
    fn workspace_sessions_get_independent_filesystems_and_repository_clones() {
        let source = git_repository();

        let backend = InProcessBackend::new();
        let connection = backend.connect();
        let capabilities = CapabilitySet::new([
            Capability::ManageWorkspaces,
            Capability::ReadAgentSession,
            Capability::CreateAgentSession,
            Capability::ReadSessionFilesystem,
            Capability::WriteSessionFilesystem,
            Capability::ManageSessionRepositories,
            Capability::ForkAgentSession,
            Capability::ReadVcsStatus,
        ]);
        let negotiated = connection.request(RequestEnvelope::new(ClientRequest::Negotiate {
            client_version: CURRENT_PROTOCOL_VERSION,
            capabilities,
        }));
        assert!(matches!(
            negotiated.result,
            Ok(ServerResponse::Negotiated(_))
        ));

        let workspace = connection.request(RequestEnvelope::new(ClientRequest::CreateWorkspace {
            name: "Isolation test".to_owned(),
        }));
        let Ok(ServerResponse::WorkspaceCreated(workspace)) = workspace.result else {
            panic!("expected workspace creation");
        };
        let create_session = |name: &str| {
            let response = connection.request(RequestEnvelope::new(
                ClientRequest::CreateAgentSessionInWorkspace {
                    workspace_id: workspace.id,
                    name: name.to_owned(),
                },
            ));
            let Ok(ServerResponse::AgentSessionCreated(session)) = response.result else {
                panic!("expected session creation");
            };
            session
        };
        let first = create_session("First");
        let second = create_session("Second");
        let attach_repository = |session_id| {
            let response = connection.request(RequestEnvelope::new(
                ClientRequest::AttachSessionRepository {
                    session_id,
                    source: source.display().to_string(),
                    path: "repo".to_owned(),
                    revision: None,
                },
            ));
            let Ok(ServerResponse::SessionRepositoryAttached(repository)) = response.result else {
                panic!("expected repository attachment");
            };
            repository
        };
        let first_repository = attach_repository(first.id);
        let second_repository = attach_repository(second.id);
        assert_ne!(first_repository.id, second_repository.id);

        let edit = connection.request(RequestEnvelope::new(
            ClientRequest::ApplySessionFilesystemEdit {
                session_id: first.id,
                edit: WorkspaceEdit {
                    path: "repo/README.md".to_owned(),
                    old_text: "source".to_owned(),
                    new_text: "first session".to_owned(),
                    expected_revision: None,
                },
            },
        ));
        assert!(matches!(
            edit.result,
            Ok(ServerResponse::WorkspaceEditApplied(_))
        ));

        let fork = connection.request(RequestEnvelope::new(ClientRequest::ForkAgentSession {
            session_id: first.id,
            name: "Forked first".to_owned(),
        }));
        let Ok(ServerResponse::AgentSessionForked(fork)) = fork.result else {
            panic!("expected forked session");
        };
        let repositories = connection.request(RequestEnvelope::new(
            ClientRequest::ListSessionRepositories {
                session_id: fork.id,
            },
        ));
        let Ok(ServerResponse::SessionRepositories { repositories }) = repositories.result else {
            panic!("expected forked repositories");
        };
        let fork_repository = repositories.first().expect("repository was copied");
        assert_ne!(fork_repository.id, first_repository.id);
        let fork_edit = connection.request(RequestEnvelope::new(
            ClientRequest::ApplySessionFilesystemEdit {
                session_id: fork.id,
                edit: WorkspaceEdit {
                    path: "repo/README.md".to_owned(),
                    old_text: "first session".to_owned(),
                    new_text: "forked session".to_owned(),
                    expected_revision: None,
                },
            },
        ));
        assert!(matches!(
            fork_edit.result,
            Ok(ServerResponse::WorkspaceEditApplied(_))
        ));
        for (session_id, expected_content) in
            [(first.id, "first session\n"), (fork.id, "forked session\n")]
        {
            let file = connection.request(RequestEnvelope::new(ClientRequest::ReadSessionFile {
                session_id,
                path: "repo/README.md".to_owned(),
            }));
            let Ok(ServerResponse::SessionFilesystemFile(file)) = file.result else {
                panic!("expected session file");
            };
            assert_eq!(file.content, expected_content);
        }

        for (session_id, expected_content, repository_id, expected_clean) in [
            (first.id, "first session\n", first_repository.id, false),
            (second.id, "source\n", second_repository.id, true),
        ] {
            let file = connection.request(RequestEnvelope::new(ClientRequest::ReadSessionFile {
                session_id,
                path: "repo/README.md".to_owned(),
            }));
            let Ok(ServerResponse::SessionFilesystemFile(file)) = file.result else {
                panic!("expected session file");
            };
            assert_eq!(file.content, expected_content);

            let status =
                connection.request(RequestEnvelope::new(ClientRequest::GetSessionVcsStatus {
                    session_id,
                    repository_id,
                }));
            let Ok(ServerResponse::VcsStatus(status)) = status.result else {
                panic!("expected repository status");
            };
            assert_eq!(status.clean, expected_clean);
        }
        assert_eq!(
            fs::read_to_string(source.join("README.md")).unwrap(),
            "source\n"
        );
        fs::remove_dir_all(&backend.session_root_base).unwrap();
        fs::remove_dir_all(source).unwrap();
    }

    #[test]
    fn session_filesystem_and_repository_metadata_survive_restart() {
        let source = git_repository();
        let state_dir = workspace();
        let persistence = state_dir.join("backend.sqlite");
        let (workspace_id, session_id) = {
            let backend = InProcessBackend::new_persistent(&persistence).unwrap();
            let connection = backend.connect();
            let capabilities = CapabilitySet::new([
                Capability::ManageWorkspaces,
                Capability::ReadAgentSession,
                Capability::CreateAgentSession,
                Capability::ReadSessionFilesystem,
                Capability::WriteSessionFilesystem,
                Capability::ManageSessionRepositories,
                Capability::ReadVcsStatus,
            ]);
            let negotiated = connection.request(RequestEnvelope::new(ClientRequest::Negotiate {
                client_version: CURRENT_PROTOCOL_VERSION,
                capabilities,
            }));
            assert!(matches!(
                negotiated.result,
                Ok(ServerResponse::Negotiated(_))
            ));
            let created =
                connection.request(RequestEnvelope::new(ClientRequest::CreateWorkspace {
                    name: "Persistent workspace".to_owned(),
                }));
            let Ok(ServerResponse::WorkspaceCreated(workspace)) = created.result else {
                panic!("expected workspace creation");
            };
            let created = connection.request(RequestEnvelope::new(
                ClientRequest::CreateAgentSessionInWorkspace {
                    workspace_id: workspace.id,
                    name: "Persistent session".to_owned(),
                },
            ));
            let Ok(ServerResponse::AgentSessionCreated(session)) = created.result else {
                panic!("expected session creation");
            };
            let attached = connection.request(RequestEnvelope::new(
                ClientRequest::AttachSessionRepository {
                    session_id: session.id,
                    source: source.display().to_string(),
                    path: "repo".to_owned(),
                    revision: None,
                },
            ));
            assert!(
                matches!(
                    attached.result,
                    Ok(ServerResponse::SessionRepositoryAttached(_))
                ),
                "{:?}",
                attached.result
            );
            let edit = connection.request(RequestEnvelope::new(
                ClientRequest::ApplySessionFilesystemEdit {
                    session_id: session.id,
                    edit: WorkspaceEdit {
                        path: "repo/README.md".to_owned(),
                        old_text: "source".to_owned(),
                        new_text: "persisted session edit".to_owned(),
                        expected_revision: None,
                    },
                },
            ));
            assert!(matches!(
                edit.result,
                Ok(ServerResponse::WorkspaceEditApplied(_))
            ));
            (workspace.id, session.id)
        };

        {
            let backend = InProcessBackend::new_persistent(&persistence).unwrap();
            let connection = backend.connect();
            let capabilities = CapabilitySet::new([
                Capability::ReadAgentSession,
                Capability::ReadSessionFilesystem,
                Capability::ReadVcsStatus,
            ]);
            let negotiated = connection.request(RequestEnvelope::new(ClientRequest::Negotiate {
                client_version: CURRENT_PROTOCOL_VERSION,
                capabilities,
            }));
            assert!(matches!(
                negotiated.result,
                Ok(ServerResponse::Negotiated(_))
            ));
            let sessions =
                connection.request(RequestEnvelope::new(ClientRequest::ListWorkspaceSessions {
                    workspace_id,
                    include_archived: false,
                }));
            assert!(matches!(
                sessions.result,
                Ok(ServerResponse::AgentSessions { sessions })
                    if sessions.iter().any(|session| session.id == session_id)
            ));
            let repositories = connection.request(RequestEnvelope::new(
                ClientRequest::ListSessionRepositories { session_id },
            ));
            let Ok(ServerResponse::SessionRepositories { repositories }) = repositories.result
            else {
                panic!("expected restored repository metadata");
            };
            let repository = repositories.first().expect("repository was restored");
            let file = connection.request(RequestEnvelope::new(ClientRequest::ReadSessionFile {
                session_id,
                path: "repo/README.md".to_owned(),
            }));
            let Ok(ServerResponse::SessionFilesystemFile(file)) = file.result else {
                panic!("expected restored session file");
            };
            assert_eq!(file.content, "persisted session edit\n");
            let status =
                connection.request(RequestEnvelope::new(ClientRequest::GetSessionVcsStatus {
                    session_id,
                    repository_id: repository.id,
                }));
            let Ok(ServerResponse::VcsStatus(status)) = status.result else {
                panic!("expected restored repository status");
            };
            assert!(!status.clean);
        }

        fs::remove_dir_all(source).unwrap();
        fs::remove_dir_all(persistence.with_extension("session-roots")).unwrap();
        fs::remove_dir_all(state_dir).unwrap();
    }

    #[test]
    fn m5_session_projections_reconnect_and_archive_authoritatively() {
        let root = git_repository();
        let backend = InProcessBackend::new();
        let connection = backend.connect();
        negotiate_m5(&connection);
        let created = connection.request(RequestEnvelope::new(ClientRequest::CreateWorkspace {
            name: "Navigator".to_owned(),
        }));
        let Ok(ServerResponse::WorkspaceCreated(workspace)) = created.result else {
            panic!("expected workspace creation");
        };
        let created = connection.request(RequestEnvelope::new(
            ClientRequest::CreateAgentSessionInWorkspace {
                workspace_id: workspace.id,
                name: "Navigator session".to_owned(),
            },
        ));
        let session = match created.result.unwrap() {
            ServerResponse::AgentSessionCreated(snapshot) => snapshot,
            response => panic!("unexpected response: {response:?}"),
        };

        let attached = connection.request(RequestEnvelope::new(
            ClientRequest::AttachSessionRepository {
                session_id: session.id,
                source: root.display().to_string(),
                path: "repo".to_owned(),
                revision: None,
            },
        ));
        assert!(matches!(
            attached.result,
            Ok(ServerResponse::SessionRepositoryAttached(_))
        ));
        let workspaces = connection.request(RequestEnvelope::new(ClientRequest::ListWorkspaces));
        let ServerResponse::Workspaces { workspaces } = workspaces.result.unwrap() else {
            panic!("unexpected workspace response");
        };
        assert_eq!(workspaces, vec![workspace.clone()]);

        let renamed = connection.request(RequestEnvelope::new(ClientRequest::RenameAgentSession {
            session_id: session.id,
            name: "Renamed session".to_owned(),
        }));
        let session = match renamed.result.unwrap() {
            ServerResponse::AgentSessionRenamed(snapshot) => snapshot,
            response => panic!("unexpected rename response: {response:?}"),
        };
        assert_eq!(session.name, "Renamed session");

        let started =
            connection.request(RequestEnvelope::new(ClientRequest::StartSessionAgentRun {
                session_id: session.id,
                task: "inspect the workspace".to_owned(),
                model: loom_model::ModelId::new("deterministic/demo"),
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

        let changes = connection.request(RequestEnvelope::new(
            ClientRequest::GetSessionFilesystemChanges {
                session_id: session.id,
                after_sequence: None,
            },
        ));
        assert!(matches!(
            changes.result,
            Ok(ServerResponse::SessionFilesystemChanges { .. })
        ));

        let archived =
            connection.request(RequestEnvelope::new(ClientRequest::ArchiveAgentSession {
                session_id: session.id,
            }));
        assert!(matches!(
            archived.result,
            Ok(ServerResponse::AgentSessionArchived(_))
        ));
        let sessions =
            connection.request(RequestEnvelope::new(ClientRequest::ListWorkspaceSessions {
                workspace_id: workspace.id,
                include_archived: false,
            }));
        let ServerResponse::AgentSessions { sessions } = sessions.result.unwrap() else {
            panic!("unexpected session list response");
        };
        assert!(sessions.is_empty());
        fs::remove_dir_all(&backend.session_root_base).unwrap();
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn creates_session_and_reads_event_stream() {
        let backend = InProcessBackend::new();
        let connection = backend.connect();
        negotiate(&connection);

        let workspace = connection.request(RequestEnvelope::new(ClientRequest::CreateWorkspace {
            name: "In-process workspace".to_owned(),
        }));
        let ServerResponse::WorkspaceCreated(workspace) = workspace.result.unwrap() else {
            panic!("unexpected workspace response");
        };
        let create = connection.request(RequestEnvelope::new(
            ClientRequest::CreateAgentSessionInWorkspace {
                workspace_id: workspace.id,
                name: "In-process demo".to_owned(),
            },
        ));
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
        let root = git_repository();
        let backend = InProcessBackend::new();
        let connection = backend.connect();
        negotiate(&connection);
        let workspace = connection.request(RequestEnvelope::new(ClientRequest::CreateWorkspace {
            name: "M1 workspace".to_owned(),
        }));
        let ServerResponse::WorkspaceCreated(workspace) = workspace.result.unwrap() else {
            panic!("unexpected workspace response");
        };
        let session = connection.request(RequestEnvelope::new(
            ClientRequest::CreateAgentSessionInWorkspace {
                workspace_id: workspace.id,
                name: "M1 run".to_owned(),
            },
        ));
        let session_id = match session.result.unwrap() {
            ServerResponse::AgentSessionCreated(snapshot) => snapshot.id,
            response => panic!("unexpected response: {response:?}"),
        };
        connection.request(RequestEnvelope::new(
            ClientRequest::SetSessionApprovalPolicy {
                session_id,
                policy: ApprovalPolicy::default(),
                auto_approve_actions: Some(false),
            },
        ));
        let attached = connection.request(RequestEnvelope::new(
            ClientRequest::AttachSessionRepository {
                session_id,
                source: root.display().to_string(),
                path: "repo".to_owned(),
                revision: None,
            },
        ));
        assert!(
            matches!(
                attached.result,
                Ok(ServerResponse::SessionRepositoryAttached(_))
            ),
            "{:?}",
            attached.result
        );
        let started =
            connection.request(RequestEnvelope::new(ClientRequest::StartSessionAgentRun {
                session_id,
                task: "create a demo file".to_owned(),
                model: ModelId::new("deterministic/demo"),
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
        assert!(
            backend
                .session_root_base
                .join(workspace.id.to_string())
                .join(session_id.to_string())
                .join("fs/loom-m1-demo.txt")
                .is_file()
        );
        fs::remove_dir_all(&backend.session_root_base).unwrap();
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn requires_negotiation_before_session_requests() {
        let backend = InProcessBackend::new();
        let connection = backend.connect();
        let response = connection.request(RequestEnvelope::new(ClientRequest::ListWorkspaces));

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
        let root = git_repository();
        let backend = InProcessBackend::new();
        let connection = backend.connect();
        negotiate_m2(&connection);
        let workspace = connection.request(RequestEnvelope::new(ClientRequest::CreateWorkspace {
            name: "Filesystem controls".to_owned(),
        }));
        let ServerResponse::WorkspaceCreated(workspace) = workspace.result.unwrap() else {
            panic!("unexpected workspace response");
        };
        let created = connection.request(RequestEnvelope::new(
            ClientRequest::CreateAgentSessionInWorkspace {
                workspace_id: workspace.id,
                name: "Filesystem controls".to_owned(),
            },
        ));
        let ServerResponse::AgentSessionCreated(session) = created.result.unwrap() else {
            panic!("unexpected session response");
        };
        let session_id = session.id;
        let attached = connection.request(RequestEnvelope::new(
            ClientRequest::AttachSessionRepository {
                session_id,
                source: root.display().to_string(),
                path: "repo".to_owned(),
                revision: None,
            },
        ));
        assert!(matches!(
            attached.result,
            Ok(ServerResponse::SessionRepositoryAttached(_))
        ));
        let snapshot = connection.request(RequestEnvelope::new(
            ClientRequest::GetSessionFilesystemSnapshot { session_id },
        ));
        let ServerResponse::SessionFilesystemSnapshot(snapshot) = snapshot.result.unwrap() else {
            panic!("unexpected filesystem snapshot");
        };
        assert!(
            snapshot
                .entries
                .iter()
                .any(|entry| entry.path == "repo/README.md")
        );
        let file = connection.request(RequestEnvelope::new(ClientRequest::ReadSessionFile {
            session_id,
            path: "repo/README.md".to_owned(),
        }));
        let revision = match file.result.unwrap() {
            ServerResponse::SessionFilesystemFile(file) => file.revision,
            response => panic!("unexpected response: {response:?}"),
        };
        let checkpoint = connection.request(RequestEnvelope::new(
            ClientRequest::CreateSessionCheckpoint {
                session_id,
                label: "before user edit".to_owned(),
            },
        ));
        let checkpoint_id = match checkpoint.result.unwrap() {
            ServerResponse::CheckpointCreated(checkpoint) => checkpoint.id,
            response => panic!("unexpected response: {response:?}"),
        };
        let edit = connection.request(RequestEnvelope::new(
            ClientRequest::ApplySessionFilesystemEdit {
                session_id,
                edit: WorkspaceEdit {
                    path: "repo/README.md".to_owned(),
                    old_text: "source".to_owned(),
                    new_text: "user".to_owned(),
                    expected_revision: Some(revision),
                },
            },
        ));
        assert!(matches!(
            edit.result,
            Ok(ServerResponse::WorkspaceEditApplied(_))
        ));
        let changes = connection.request(RequestEnvelope::new(
            ClientRequest::GetSessionFilesystemChanges {
                session_id,
                after_sequence: None,
            },
        ));
        let ServerResponse::SessionFilesystemChanges { changes, .. } = changes.result.unwrap()
        else {
            panic!("unexpected filesystem changes response");
        };
        assert!(changes.iter().any(|event| event.path == "repo/README.md"));
        let revert = connection.request(RequestEnvelope::new(
            ClientRequest::RevertSessionCheckpoint {
                session_id,
                checkpoint_id,
            },
        ));
        assert_eq!(
            revert.result.unwrap_err().code,
            ErrorCode::Conflict,
            "checkpoint revert must preserve the intervening user edit"
        );
        let file = connection.request(RequestEnvelope::new(ClientRequest::ReadSessionFile {
            session_id,
            path: "repo/README.md".to_owned(),
        }));
        assert!(matches!(
            file.result,
            Ok(ServerResponse::SessionFilesystemFile(file)) if file.content == "user\n"
        ));

        let terminal_command = if cfg!(windows) {
            (
                "cmd".to_owned(),
                vec!["/C".to_owned(), "echo terminal".to_owned()],
            )
        } else {
            ("printf".to_owned(), vec!["terminal".to_owned()])
        };
        let terminal =
            connection.request(RequestEnvelope::new(ClientRequest::OpenSessionTerminal {
                session_id,
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
            let events = connection.request(RequestEnvelope::new(
                ClientRequest::GetSessionTerminalEvents {
                    session_id,
                    terminal_id,
                    after_sequence: None,
                },
            ));
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
        let task = connection.request(RequestEnvelope::new(ClientRequest::StartSessionTask {
            session_id,
            spec: TaskSpec {
                kind: TaskKind::Test,
                label: "M2 task".to_owned(),
                command: task_command.0,
                args: task_command.1,
                cwd: Some("repo".to_owned()),
                output_limit_bytes: Some(4096),
                artifact_paths: vec!["repo/artifact.txt".to_owned()],
            },
        }));
        let task_id = match task.result.unwrap() {
            ServerResponse::TaskStarted(snapshot) => snapshot.id,
            response => panic!("unexpected response: {response:?}"),
        };
        let mut task_done = false;
        for _ in 0..100 {
            let current = connection.request(RequestEnvelope::new(ClientRequest::GetSessionTask {
                session_id,
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
        let listed = connection.request(RequestEnvelope::new(ClientRequest::ListSessionTasks {
            session_id,
        }));
        let ServerResponse::Tasks { tasks } = listed.result.unwrap() else {
            panic!("unexpected task list response");
        };
        assert!(tasks.iter().any(|task| task.id == task_id));
        let task_events =
            connection.request(RequestEnvelope::new(ClientRequest::GetSessionTaskEvents {
                session_id,
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

        let control = connection.request(RequestEnvelope::new(
            ClientRequest::TakeSessionFilesystemControl {
                session_id,
                control: WorkspaceControl::User,
            },
        ));
        assert!(matches!(
            control.result,
            Ok(ServerResponse::WorkspaceControl(WorkspaceControl::User))
        ));
        fs::remove_dir_all(&backend.session_root_base).unwrap();
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn policy_decisions_are_visible_and_can_stop_agent_writes() {
        let backend = InProcessBackend::new();
        let connection = backend.connect();
        negotiate_m2(&connection);
        let workspace = connection.request(RequestEnvelope::new(ClientRequest::CreateWorkspace {
            name: "Policy workspace".to_owned(),
        }));
        let ServerResponse::WorkspaceCreated(workspace) = workspace.result.unwrap() else {
            panic!("unexpected workspace response");
        };
        let session = connection.request(RequestEnvelope::new(
            ClientRequest::CreateAgentSessionInWorkspace {
                workspace_id: workspace.id,
                name: "M2 policy".to_owned(),
            },
        ));
        let session_id = match session.result.unwrap() {
            ServerResponse::AgentSessionCreated(snapshot) => snapshot.id,
            response => panic!("unexpected response: {response:?}"),
        };
        let default_settings = connection.request(RequestEnvelope::new(
            ClientRequest::GetAgentSessionSnapshot { session_id },
        ));
        let ServerResponse::AgentSessionSnapshot(default_settings) =
            default_settings.result.unwrap()
        else {
            panic!("unexpected session snapshot response");
        };
        assert!(default_settings.auto_approve_actions);
        assert_eq!(
            default_settings.approval_policy,
            loom_core::ApprovalPolicy::auto_approve()
        );
        let other_session = connection.request(RequestEnvelope::new(
            ClientRequest::CreateAgentSessionInWorkspace {
                workspace_id: workspace.id,
                name: "Other session".to_owned(),
            },
        ));
        let other_session_id = match other_session.result.unwrap() {
            ServerResponse::AgentSessionCreated(snapshot) => snapshot.id,
            response => panic!("unexpected response: {response:?}"),
        };
        let policy = loom_core::ApprovalPolicy {
            write: PolicyDecision::Deny,
            ..Default::default()
        };
        let policy_response = connection.request(RequestEnvelope::new(
            ClientRequest::SetSessionApprovalPolicy {
                session_id,
                policy,
                auto_approve_actions: Some(false),
            },
        ));
        assert!(matches!(
            policy_response.result,
            Ok(ServerResponse::ApprovalPolicy(_))
        ));
        let other_settings = connection.request(RequestEnvelope::new(
            ClientRequest::GetAgentSessionSnapshot {
                session_id: other_session_id,
            },
        ));
        let ServerResponse::AgentSessionSnapshot(other_settings) = other_settings.result.unwrap()
        else {
            panic!("unexpected session snapshot response");
        };
        assert!(other_settings.auto_approve_actions);
        assert_eq!(
            other_settings.approval_policy,
            loom_core::ApprovalPolicy::auto_approve()
        );
        let started =
            connection.request(RequestEnvelope::new(ClientRequest::StartSessionAgentRun {
                session_id,
                task: "attempt a write".to_owned(),
                model: ModelId::new("deterministic/demo"),
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
        fs::remove_dir_all(&backend.session_root_base).unwrap();
    }

    #[test]
    fn persistent_backend_recovers_transcript_workspace_and_pending_approval() {
        let persistence =
            std::env::temp_dir().join(format!("loom-server-state-{}.db", WorkspaceId::new()));
        let session_root_base;
        let (session_id, run_id, approval_id) = {
            let backend = InProcessBackend::new_persistent(&persistence).unwrap();
            session_root_base = backend.session_root_base.clone();
            let connection = backend.connect();
            negotiate_m3(&connection);
            let created =
                connection.request(RequestEnvelope::new(ClientRequest::CreateWorkspace {
                    name: "Durable workspace".to_owned(),
                }));
            let ServerResponse::WorkspaceCreated(workspace) = created.result.unwrap() else {
                panic!("unexpected workspace response");
            };
            let session = connection.request(RequestEnvelope::new(
                ClientRequest::CreateAgentSessionInWorkspace {
                    workspace_id: workspace.id,
                    name: "durable run".to_owned(),
                },
            ));
            let session_id = match session.result.unwrap() {
                ServerResponse::AgentSessionCreated(snapshot) => snapshot.id,
                response => panic!("unexpected response: {response:?}"),
            };
            connection.request(RequestEnvelope::new(
                ClientRequest::SetSessionApprovalPolicy {
                    session_id,
                    policy: ApprovalPolicy::default(),
                    auto_approve_actions: Some(false),
                },
            ));
            let started =
                connection.request(RequestEnvelope::new(ClientRequest::StartSessionAgentRun {
                    session_id,
                    task: "create a demo file".to_owned(),
                    model: ModelId::new("deterministic/demo"),
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
        let recovered_session = connection.request(RequestEnvelope::new(
            ClientRequest::GetAgentSessionSnapshot { session_id },
        ));
        let ServerResponse::AgentSessionSnapshot(recovered_session) =
            recovered_session.result.unwrap()
        else {
            panic!("unexpected recovered session snapshot response");
        };
        assert!(!recovered_session.auto_approve_actions);
        assert_eq!(recovered_session.approval_policy, ApprovalPolicy::default());
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
        assert_eq!(checkpoint.session_id, session_id);

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
        let filesystem = connection.request(RequestEnvelope::new(
            ClientRequest::GetSessionFilesystemSnapshot { session_id },
        ));
        assert!(matches!(
            filesystem.result,
            Ok(ServerResponse::SessionFilesystemSnapshot(_))
        ));
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
        fs::remove_dir_all(session_root_base).unwrap();
    }

    #[test]
    fn pause_resume_fork_and_provider_discovery_are_protocol_operations() {
        let backend = InProcessBackend::new();
        let connection = backend.connect();
        negotiate_m3(&connection);
        let workspace = connection.request(RequestEnvelope::new(ClientRequest::CreateWorkspace {
            name: "Control workspace".to_owned(),
        }));
        let ServerResponse::WorkspaceCreated(workspace) = workspace.result.unwrap() else {
            panic!("unexpected workspace response");
        };
        let session = connection.request(RequestEnvelope::new(
            ClientRequest::CreateAgentSessionInWorkspace {
                workspace_id: workspace.id,
                name: "control run".to_owned(),
            },
        ));
        let session_id = match session.result.unwrap() {
            ServerResponse::AgentSessionCreated(snapshot) => snapshot.id,
            response => panic!("unexpected response: {response:?}"),
        };
        connection.request(RequestEnvelope::new(
            ClientRequest::SetSessionApprovalPolicy {
                session_id,
                policy: ApprovalPolicy::default(),
                auto_approve_actions: Some(false),
            },
        ));
        let started =
            connection.request(RequestEnvelope::new(ClientRequest::StartSessionAgentRun {
                session_id,
                task: "control".to_owned(),
                model: ModelId::new("deterministic/demo"),
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
        let forked_settings = connection.request(RequestEnvelope::new(
            ClientRequest::GetAgentSessionSnapshot {
                session_id: forked_id,
            },
        ));
        let ServerResponse::AgentSessionSnapshot(forked_settings) = forked_settings.result.unwrap()
        else {
            panic!("unexpected forked session snapshot response");
        };
        assert!(!forked_settings.auto_approve_actions);
        assert_eq!(forked_settings.approval_policy, ApprovalPolicy::default());

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
        fs::remove_dir_all(&backend.session_root_base).unwrap();
    }

    #[test]
    fn explicit_limits_and_context_inspection_are_durable_protocol_state() {
        let backend = InProcessBackend::new();
        let connection = backend.connect();
        negotiate_m3(&connection);
        let workspace = connection.request(RequestEnvelope::new(ClientRequest::CreateWorkspace {
            name: "Limited workspace".to_owned(),
        }));
        let ServerResponse::WorkspaceCreated(workspace) = workspace.result.unwrap() else {
            panic!("unexpected workspace response");
        };
        let session = connection.request(RequestEnvelope::new(
            ClientRequest::CreateAgentSessionInWorkspace {
                workspace_id: workspace.id,
                name: "limited run".to_owned(),
            },
        ));
        let session_id = match session.result.unwrap() {
            ServerResponse::AgentSessionCreated(snapshot) => snapshot.id,
            response => panic!("unexpected response: {response:?}"),
        };
        let started = connection.request(RequestEnvelope::new(
            ClientRequest::StartSessionAgentRunWithOptions {
                session_id,
                task: "limited".to_owned(),
                model: ModelId::new("deterministic/demo"),
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
        fs::remove_dir_all(&backend.session_root_base).unwrap();
    }

    #[test]
    fn non_sqlite_persistence_file_is_rejected_without_fallback() {
        let path =
            std::env::temp_dir().join(format!("loom-server-malformed-{}.db", WorkspaceId::new()));
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
        let workspace = connection.request(RequestEnvelope::new(ClientRequest::CreateWorkspace {
            name: "Context workspace".to_owned(),
        }));
        let ServerResponse::WorkspaceCreated(workspace) = workspace.result.unwrap() else {
            panic!("unexpected workspace response");
        };
        let created = connection.request(RequestEnvelope::new(
            ClientRequest::CreateAgentSessionInWorkspace {
                workspace_id: workspace.id,
                name: "Context session".to_owned(),
            },
        ));
        let ServerResponse::AgentSessionCreated(session) = created.result.unwrap() else {
            panic!("unexpected session response");
        };
        let session_id = session.id;
        let attached = connection.request(RequestEnvelope::new(
            ClientRequest::AttachSessionRepository {
                session_id,
                source: root.display().to_string(),
                path: "repo".to_owned(),
                revision: None,
            },
        ));
        assert!(matches!(
            attached.result,
            Ok(ServerResponse::SessionRepositoryAttached(_))
        ));
        let context = connection.request(RequestEnvelope::new(
            ClientRequest::GetSessionContextFiles { session_id },
        ));
        assert!(matches!(
            context.result,
            Ok(ServerResponse::ContextFiles { .. })
        ));
        let repositories = connection.request(RequestEnvelope::new(
            ClientRequest::ListSessionRepositories { session_id },
        ));
        let ServerResponse::SessionRepositories { repositories } = repositories.result.unwrap()
        else {
            panic!("unexpected session repositories");
        };
        let vcs = connection.request(RequestEnvelope::new(ClientRequest::GetSessionVcsStatus {
            session_id,
            repository_id: repositories[0].id,
        }));
        assert!(matches!(vcs.result, Ok(ServerResponse::VcsStatus(_))));

        let task = connection.request(RequestEnvelope::new(ClientRequest::StartSessionTask {
            session_id,
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
            let current = connection.request(RequestEnvelope::new(ClientRequest::GetSessionTask {
                session_id,
                task_id,
            }));
            if let Ok(ServerResponse::Task(snapshot)) = current.result
                && matches!(
                    snapshot.status,
                    TaskStatus::Completed | TaskStatus::Failed | TaskStatus::Cancelled
                )
            {
                let evidence = connection.request(RequestEnvelope::new(
                    ClientRequest::GetSessionTaskEvidence {
                        session_id,
                        task_id,
                    },
                ));
                assert!(matches!(
                    evidence.result,
                    Ok(ServerResponse::TaskEvidence { .. })
                ));
                fs::remove_dir_all(&backend.session_root_base).unwrap();
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
        let (endpoint, started) = slow_model_endpoint();
        let backend =
            InProcessBackend::with_openai_compatible(endpoint, "key", ModelId::new("slow/model"));
        let connection = backend.connect();
        negotiate_m3(&connection);
        let workspace = connection.request(RequestEnvelope::new(ClientRequest::CreateWorkspace {
            name: "Interruptible workspace".to_owned(),
        }));
        let ServerResponse::WorkspaceCreated(workspace) = workspace.result.unwrap() else {
            panic!("unexpected workspace response");
        };
        let session = connection.request(RequestEnvelope::new(
            ClientRequest::CreateAgentSessionInWorkspace {
                workspace_id: workspace.id,
                name: "interruptible run".to_owned(),
            },
        ));
        let session_id = match session.result.unwrap() {
            ServerResponse::AgentSessionCreated(snapshot) => snapshot.id,
            response => panic!("unexpected response: {response:?}"),
        };
        let started_run =
            connection.request(RequestEnvelope::new(ClientRequest::StartSessionAgentRun {
                session_id,
                task: "stream for a long time".to_owned(),
                model: ModelId::new("slow/model"),
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
        fs::remove_dir_all(&backend.session_root_base).unwrap();
    }

    #[test]
    fn a_running_model_call_can_be_paused_and_resumed() {
        let (endpoint, started) = slow_model_endpoint();
        let backend =
            InProcessBackend::with_openai_compatible(endpoint, "key", ModelId::new("slow/model"));
        let connection = backend.connect();
        negotiate_m3(&connection);
        let workspace = connection.request(RequestEnvelope::new(ClientRequest::CreateWorkspace {
            name: "Pausable workspace".to_owned(),
        }));
        let ServerResponse::WorkspaceCreated(workspace) = workspace.result.unwrap() else {
            panic!("unexpected workspace response");
        };
        let session = connection.request(RequestEnvelope::new(
            ClientRequest::CreateAgentSessionInWorkspace {
                workspace_id: workspace.id,
                name: "pausable run".to_owned(),
            },
        ));
        let session_id = match session.result.unwrap() {
            ServerResponse::AgentSessionCreated(snapshot) => snapshot.id,
            response => panic!("unexpected response: {response:?}"),
        };
        let started_run =
            connection.request(RequestEnvelope::new(ClientRequest::StartSessionAgentRun {
                session_id,
                task: "stream for a long time".to_owned(),
                model: ModelId::new("slow/model"),
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
        fs::remove_dir_all(&backend.session_root_base).unwrap();
    }

    #[test]
    fn retryable_mutation_idempotency_survives_backend_restart() {
        let path =
            std::env::temp_dir().join(format!("loom-server-idempotency-{}.db", WorkspaceId::new()));
        let request_id = loom_core::RequestId::new();
        let (workspace_id, first) = {
            let backend = InProcessBackend::new_persistent(&path).unwrap();
            let connection = backend.connect();
            negotiate(&connection);
            let created =
                connection.request(RequestEnvelope::new(ClientRequest::CreateWorkspace {
                    name: "Idempotency workspace".to_owned(),
                }));
            let ServerResponse::WorkspaceCreated(workspace) = created.result.unwrap() else {
                panic!("unexpected workspace response");
            };
            let request = RequestEnvelope::with_request_id(
                request_id,
                ClientRequest::CreateAgentSessionInWorkspace {
                    workspace_id: workspace.id,
                    name: "durable idempotency".to_owned(),
                },
            );
            (workspace.id, connection.request(request))
        };
        let backend = InProcessBackend::new_persistent(&path).unwrap();
        let connection = backend.connect();
        negotiate(&connection);
        let request = RequestEnvelope::with_request_id(
            request_id,
            ClientRequest::CreateAgentSessionInWorkspace {
                workspace_id,
                name: "durable idempotency".to_owned(),
            },
        );
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
