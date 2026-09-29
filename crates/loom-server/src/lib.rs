use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
    sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
    sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError, RwLock, Weak},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use loom_agent::{
    AgentEvent, AgentEventObserver, AgentRunSnapshot, AgentRunState, AgentRuntime,
    AgentRuntimeOptions, AgentRuntimeState, AgentTask, RunControl, RunProgress,
};
use loom_core::{
    ActivityId, AgentSessionId, AgentSessionSnapshot, AgentSessionState, ApprovalPolicy,
    Capability, CapabilitySet, DelegatedTaskSpec, ErrorCode, EventSequence, LoomError,
    MAX_PROJECT_AGENT_DEPTH, ProjectAgentRecord, ProjectId, ProjectSnapshot,
    ProjectWorktreeCleanupDisposition, ProjectWorktreeRecord, ProjectWorktreeStatus,
    ProtocolVersion, RepositoryId, RequestId, Result, RunAttemptId, SessionEventRecord,
    TaskContextReference, Timestamp, UsageSnapshot, WorkspaceId, WorkspaceRecord,
};
use loom_model::{
    ModelCapabilities, ModelDescriptor, ModelId, ModelMessage, ProviderId, ToolCall, ToolDefinition,
};
use loom_persistence::{
    DurableFeedSessionCursor, DurableFeedState, DurableFeedWorkspaceCursor, DurableFilesystemDelta,
    DurableFilesystemEdit, DurableFilesystemRecord, DurableIdempotencyRecord, DurableProviderState,
    DurableRunCheckpointWrite, DurableRunContextCheckpoint, DurableRunMessage,
    DurableRunMessageDelta, DurableRunRuntimeConfig, DurableRunSummary,
    DurableSessionProjectionRead, DurableSessionSettings, DurableStateWrite, FilePersistence,
    Persistence, ProjectCancellationCascadeRecord,
};
use loom_process::{TaskSupervisor, TerminalManager};
use loom_protocol::{
    AgentActivityRecord, AgentExecutionStateRecord, AgentRunMessageHeader,
    AgentRunSnapshotProjection, AgentRunTranscriptMessage, AgentSessionInitialState,
    AgentSessionSnapshotProjection, CURRENT_PROTOCOL_VERSION, ClientRequest, ContextRequest,
    ContextResponse, ControlRequest, ControlResponse, EventsRequest, EventsResponse,
    FilesystemRequest, FilesystemResponse, GitHubCopilotLoginStatus, GitHubRepository,
    MAX_AGENT_RUN_MESSAGE_CONTENT_RANGE_BYTES, MAX_AGENT_RUN_MESSAGE_PAGE_SIZE,
    MAX_AGENT_RUN_TRANSCRIPT_MESSAGE_BYTES, MAX_AGENT_RUN_TRANSCRIPT_PAGE_SIZE, NegotiationResult,
    ProjectChildControlAction, ProjectRequest, ProjectResponse, ProviderRequest, ProviderResponse,
    RepositoryRequest, RepositoryResponse, RequestEnvelope, ResponseEnvelope, RunRequest,
    RunResponse, ServerEvent, ServerEventEnvelope, ServerResponse, SessionDirectory,
    SessionFilesystemChange, SessionFilesystemFile, SessionFilesystemSnapshot, SessionRepository,
    SessionRequest, SessionResponse, TaskRequest, TaskResponse, TerminalRequest, TerminalResponse,
    UsageRequest, UsageResponse, WorkerNodeResources, WorkerNodeStatus, WorkspaceConfig,
    WorkspaceEvent, WorkspaceEventEnvelope, WorkspaceFeedEvent, WorkspaceRequest,
    WorkspaceResponse, unsupported_version_error,
};
use loom_providers::{
    CredentialRef, FileCredentialStore, GITHUB_COPILOT_CREDENTIAL_REF, GitHubCopilotAuthenticator,
    ModelProvider, ProviderConfig, ProviderHealth, ProviderRegistry, UnavailableProvider,
    UsageLedger, deterministic_descriptor,
};
use loom_session::{SessionManager, WorkspaceManager};
use loom_tools::{ToolExecutor, ToolExtension, ToolResult};
use loom_vcs::GitService;
use loom_workspace::Workspace;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::Value;
use sysinfo::System;

mod auth;
mod backend;
mod connection;
mod dispatch;
mod event_journal;
mod project_tools;
mod remote;
mod resource_monitor;
mod run_handle;
mod services;
mod util;

use util::*;

use services::admission::AdmissionService;
use services::credential::CredentialService;
#[cfg(test)]
use services::idempotency::{
    IDEMPOTENCY_RETENTION, LEGACY_IDEMPOTENCY_RETENTION, trim_idempotency_cache,
};
use services::idempotency::{IdempotencyRecord, IdempotencyStore};

const FEED_PRUNE_AFTER_NEW_SEQUENCES: u64 = 64;
const FEED_PRUNE_AFTER_NEW_BYTES: usize = 4 * 1024 * 1024;
const MAX_NONTERMINAL_PROJECT_TASKS: usize = 50;

#[derive(Deserialize)]
struct GitHubApiRepository {
    full_name: String,
    description: Option<String>,
    clone_url: String,
    private: bool,
    default_branch: String,
}

pub use auth::{AuthSession, AuthTokenStore, AuthorizationScope, IssuedToken};
pub use remote::{
    RemoteServer, RemoteServerConfig, RunningRemoteServer, WebSocketConnection, WebSocketTransport,
};

const DEFAULT_EVENT_RETENTION: usize = 4096;
const MAX_REVIEW_CHANGES: usize = 512;

const MAX_REVIEW_DIFF_BYTES: usize = 64 * 1024;
const MAX_REVIEW_FILE_BYTES: usize = 128 * 1024;
const MAX_RUN_MESSAGE_BYTES: usize = 32 * 1024;

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
struct EventJournal {
    next_sequence: EventSequence,
    events: Vec<ServerEventEnvelope>,
    #[serde(skip)]
    pending_events: Vec<ServerEventEnvelope>,
    #[serde(default)]
    workspace_events: Vec<WorkspaceEventEnvelope>,
    #[serde(skip)]
    pending_workspace_events: Vec<WorkspaceEventEnvelope>,
    #[serde(default = "default_event_retention")]
    retention_limit: usize,
}

#[derive(Clone, Debug)]
struct PersistedBackendState {
    sessions: loom_session::SessionManagerState,
    workspace_records: loom_session::WorkspaceManagerState,
    journal: EventJournal,
    session_policies: BTreeMap<AgentSessionId, ApprovalPolicy>,
    auto_approve_actions: BTreeMap<AgentSessionId, bool>,
    provider_configs: Vec<ProviderConfig>,
    provider_health: BTreeMap<ProviderId, ProviderHealth>,
    workspace_configs: BTreeMap<WorkspaceId, WorkspaceConfig>,
    provider_usage: UsageLedger,
    idempotency: BTreeMap<loom_core::RequestId, IdempotencyRecord>,
}

#[derive(Clone)]
struct PersistedRunSummary {
    snapshot: AgentRunSnapshot,
    usage: UsageSnapshot,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct PersistedSessionFilesystem {
    filesystem: loom_workspace::WorkspaceStateSnapshot,
    #[serde(skip)]
    repositories: BTreeMap<RepositoryId, SessionRepository>,
    #[serde(skip)]
    directories: Vec<SessionDirectory>,
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
    message_fragments: Mutex<MessageFragmentState>,
    activity_deltas: Mutex<PendingActivityDeltas>,
    message_checkpoint: Mutex<MessageCheckpointCursor>,
    event_gate: Mutex<()>,
    fragment_wake: Condvar,
    running: Mutex<bool>,
    idle: Condvar,
    failure: Mutex<Option<LoomError>>,
    worker: Mutex<Option<thread::JoinHandle<()>>>,
}

struct MessageCheckpointCursor {
    attempt_id: RunAttemptId,
    message_count: usize,
}

#[derive(Default)]
struct MessageFragmentState {
    active_message_ordinal: Option<u64>,
    positions: BTreeMap<u64, MessageFragmentPosition>,
    pending: BTreeMap<u64, PendingMessageFragments>,
    pending_bytes: usize,
    pending_since: Option<Instant>,
}

#[derive(Default)]
struct PendingActivityDeltas {
    by_id: BTreeMap<ActivityId, AgentActivityRecord>,
    appended_order: Vec<ActivityId>,
}

impl PendingActivityDeltas {
    fn ordered_values(&self) -> Vec<AgentActivityRecord> {
        let appended = self.appended_order.iter().copied().collect::<BTreeSet<_>>();
        self.by_id
            .iter()
            .filter(|(id, _)| !appended.contains(id))
            .map(|(_, activity)| activity.clone())
            .chain(
                self.appended_order
                    .iter()
                    .filter_map(|id| self.by_id.get(id).cloned()),
            )
            .collect()
    }
}

#[derive(Default)]
struct PendingMessageFragments {
    content: String,
    committed_bytes: usize,
}

#[derive(Clone, Copy)]
struct MessageFragmentPosition {
    ordinal: u64,
    fragment_ordinal: u64,
    byte_offset: u64,
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
/// Batch streamed transcript writes until this amount is buffered or the
/// oldest pending bytes have waited this long.
const MESSAGE_FRAGMENT_BATCH_BYTES: usize = 32 * 1024;
const MESSAGE_FRAGMENT_BATCH_INTERVAL: Duration = Duration::from_millis(50);

struct StartRunInput {
    session_id: AgentSessionId,
    project_task_id: Option<loom_core::TaskId>,
    task: String,
    model: ModelId,
    system_instructions: Option<String>,
    repository_instructions: Option<String>,
    options: AgentRuntimeOptions,
}

pub struct InProcessBackend {
    node_id: String,
    node_name: String,
    sessions: Mutex<SessionManager>,
    workspace_records: Mutex<loom_session::WorkspaceManager>,
    runs: Mutex<BTreeMap<loom_core::RunId, Arc<RunHandle>>>,
    persisted_runs: Mutex<BTreeMap<loom_core::RunId, PersistedRunSummary>>,
    journal: Mutex<EventJournal>,
    last_feed_pruned_sequence: AtomicU64,
    feed_bytes_since_prune: AtomicUsize,
    session_filesystems: Mutex<BTreeMap<AgentSessionId, Workspace>>,
    persisted_session_filesystems: Mutex<BTreeSet<AgentSessionId>>,
    session_filesystem_restore: Mutex<()>,
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
    providers: ProviderRegistry,
    credentials: CredentialService,
    persistence: Option<Arc<dyn Persistence>>,
    session_root_base: PathBuf,
    idempotency_store: IdempotencyStore,
    admissions: AdmissionService,
    self_reference: Mutex<Weak<InProcessBackend>>,
    request_lifecycle: RwLock<u8>,
    persistence_failed: AtomicBool,
    state_persist_gate: Mutex<()>,
    #[cfg(test)]
    fail_next_state_save: AtomicBool,
    #[cfg(test)]
    project_cancellation_failpoint: AtomicUsize,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DelegateProjectTaskArguments {
    child_name: String,
    intent: String,
    #[serde(default)]
    model_id: Option<String>,
    #[serde(default)]
    context_references: Vec<TaskContextReference>,
    #[serde(default)]
    dependencies: Vec<loom_core::TaskId>,
    #[serde(default)]
    permissions: loom_core::ProjectAgentPermissions,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SendProjectAgentMessageArguments {
    #[serde(default)]
    target_session_id: Option<AgentSessionId>,
    /// Optional task context for this message; it does not select a recipient.
    #[serde(default)]
    task_id: Option<loom_core::TaskId>,
    kind: loom_core::AgentMessageKind,
    body: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ListProjectChildrenArguments {}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ListProjectMessageRecipientsArguments {}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WaitForProjectChildrenArguments {
    task_ids: Vec<loom_core::TaskId>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ControlProjectChildArguments {
    task_id: loom_core::TaskId,
    action: ProjectChildControlAction,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReviewProjectChildArguments {
    task_id: loom_core::TaskId,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct IntegrateProjectChildArguments {
    task_id: loom_core::TaskId,
    expected_parent_revision: String,
}

#[derive(Clone, Copy, Default)]
struct ProjectAgentToolGrants {
    delegation: bool,
    messaging: bool,
    branch_messaging: bool,
    inspection: bool,
    child_control: bool,
    worktree: bool,
    review: bool,
    integration: bool,
}

#[derive(Clone, Copy)]
enum ProjectAgentPermission {
    Delegation,
    BranchMessaging,
    ChildControl,
    Inspection,
    WorktreeCreation,
    Review,
    Integration,
}

impl ProjectAgentPermission {
    fn is_granted(self, permissions: loom_core::ProjectAgentPermissions) -> bool {
        match self {
            Self::Delegation => permissions.delegation,
            Self::BranchMessaging => permissions.branch_messaging,
            Self::ChildControl => permissions.child_control,
            Self::Inspection => permissions.inspection,
            Self::WorktreeCreation => permissions.worktree_creation,
            Self::Review => permissions.review,
            Self::Integration => permissions.integration,
        }
    }
}

struct ProjectAgentTools {
    backend: Weak<InProcessBackend>,
    session_id: AgentSessionId,
    project_id: ProjectId,
    model_id: ModelId,
    can_delegate: bool,
    can_delegate_code: bool,
    can_message: bool,
    can_branch_message: bool,
    can_inspect_children: bool,
    can_wait_children: bool,
    can_control_children: bool,
    can_review_children: bool,
    can_integrate_children: bool,
}

#[derive(Default)]
struct ResourceMonitor {
    system: System,
    has_cpu_baseline: bool,
}

#[derive(Clone)]
pub struct InProcessConnection {
    backend: Arc<InProcessBackend>,
    negotiated_capabilities: Arc<Mutex<Option<CapabilitySet>>>,
    auth: Option<AuthSession>,
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
mod tests;
