//! The Loom backend: the in-process backend that native clients and protocol
//! tests drive directly, plus the remote WebSocket server and the standalone
//! `loom-server` executable that serves it.
//!
//! `crates/loom-server/src/main.rs` is the operator-facing binary; the library
//! API used by the desktop client lives behind the re-exports below.

use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
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
    ProjectWorktreeBaseRefresh, ProjectWorktreeCleanupDisposition, ProjectWorktreeRecord,
    ProjectWorktreeStatus, ProtocolVersion, RepositoryId, RequestId, Result, RunAttemptId,
    SessionEventRecord, TaskContextReference, Timestamp, UsageSnapshot, WorkspaceId,
    WorkspaceRecord,
};
use loom_model::{
    ModelCapabilities, ModelDescriptor, ModelId, ModelMessage, ProviderId, ToolCall, ToolDefinition,
};
use loom_persistence::{
    DurableArchiveRetentionPolicy, DurableFeedSessionCursor, DurableFeedState,
    DurableFeedWorkspaceCursor, DurableFilesystemDelta, DurableFilesystemEdit,
    DurableFilesystemRecord, DurableIdempotencyRecord, DurableProviderState,
    DurableRunCheckpointWrite, DurableRunContextCheckpoint, DurableRunMessage,
    DurableRunMessageDelta, DurableRunRuntimeConfig, DurableRunSummary,
    DurableSessionProjectionRead, DurableSessionSettings, DurableStateWrite, FilePersistence,
    Persistence, ProjectCancellationCascadeRecord,
};
use loom_process::{TaskSupervisor, TerminalManager};
use loom_protocol::{
    AgentActivityRecord, AgentExecutionStateRecord, AgentPlanProgress, AgentRunMessageHeader,
    AgentRunSnapshotProjection, AgentRunTranscriptMessage, AgentSessionInitialState,
    AgentSessionSnapshotProjection, CURRENT_PROTOCOL_VERSION, ClientRequest, ClonedRepository,
    ContextRequest, ContextResponse, ControlRequest, ControlResponse, EventsRequest,
    EventsResponse, FilesystemRequest, FilesystemResponse, GITHUB_REPOSITORY_QUERY_MIN_CHARS,
    GitHubCopilotLoginStatus, GitHubRepository, MAX_AGENT_RUN_MESSAGE_CONTENT_RANGE_BYTES,
    MAX_AGENT_RUN_MESSAGE_PAGE_SIZE, MAX_AGENT_RUN_TRANSCRIPT_MESSAGE_BYTES,
    MAX_AGENT_RUN_TRANSCRIPT_PAGE_SIZE, NegotiationResult, ProjectChildControlAction,
    ProjectRequest, ProjectResponse, ProviderRequest, ProviderResponse, RepositoryRequest,
    RepositoryResponse, RequestEnvelope, ResponseEnvelope, RunRequest, RunResponse, ServerEvent,
    ServerEventEnvelope, ServerResponse, SessionDirectory, SessionFilesystemChange,
    SessionFilesystemFile, SessionFilesystemSnapshot, SessionRepository, SessionRequest,
    SessionResponse, TaskRequest, TaskResponse, TerminalRequest, TerminalResponse, UsageRequest,
    UsageResponse, WorkerNodeResources, WorkerNodeStatus, WorkspaceConfig, WorkspaceEvent,
    WorkspaceEventEnvelope, WorkspaceFeedEvent, WorkspaceRequest, WorkspaceResponse,
    unsupported_version_error,
};
use loom_providers::{
    CredentialRef, FileCredentialStore, GITHUB_COPILOT_CREDENTIAL_REF, GitHubCopilotAuthenticator,
    ModelProvider, ProviderConfig, ProviderHealth, ProviderRegistry, UnavailableProvider,
    UsageLedger, deterministic_descriptor,
};
use loom_session::{SessionManager, WorkspaceManager};
use loom_sessionfs::Workspace;
use loom_tools::{ToolExecutor, ToolExtension, ToolResult};
use loom_vcs::{GitService, MirrorCreation};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::Value;
use sysinfo::System;

mod auth;
mod backend;

use backend::{
    CachedMirrorRefresh, MirrorFreshness, ProcessService, RepositoryService, RunService,
    SessionFilesystemService, SessionService, WorkspaceService,
};
mod connection;
mod deployment;
mod dispatch;
mod event_journal;
mod fail_stop;
mod instance;
mod project_tools;
mod remote;
mod resource_monitor;
mod retention;
mod run_executor;
mod run_handle;
mod serve;
mod services;
mod util;

use run_executor::RunExecutor;
use util::*;

use services::access::AccessService;
pub use services::access::ConnectionPresence;
use services::admission::AdmissionService;
use services::collaboration::{CollaborationService, run_revision};
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

#[derive(Deserialize)]
struct GitHubApiUser {
    login: String,
    id: u64,
    name: Option<String>,
}

pub use auth::{
    AuthSession, AuthTokenStore, AuthorizationScope, IssuedToken, ParticipantTokenEntry,
    parse_participant_tokens, participant_id_for_token, validate_display_name,
};
pub use deployment::{
    DEFAULT_DEPLOYMENT_ROLE, DEPLOYMENT_ROLE_ENV, DiagnosticsSettings, EGRESS_POLICY,
    ExecutionContext, ProcessAccount, ProcessContext, TlsState, diagnostics_lines, startup_summary,
};
pub use fail_stop::{ReopenBackoff, fail_stop_record_path, reopen_after_fail_stop};
pub use instance::{
    Instance, InstanceLayout, InstanceOptions, ResolvedToken, STATE_DB_FILE, TOKEN_FILE,
    TokenSource,
};
pub use remote::{
    DEFAULT_MAX_FRAME_BYTES, DEFAULT_MAX_IN_FLIGHT_CONTROL_REQUESTS,
    DEFAULT_MAX_IN_FLIGHT_REQUESTS, MAX_FRAME_BYTES_ENV, MAX_FRAME_BYTES_FLAG, RemoteServer,
    RemoteServerConfig, RunningRemoteServer, ServerTlsConfig, WEBSOCKET_TLS_SUPPORTED,
    WebSocketConnection, WebSocketTransport, parse_max_frame_bytes, resolve_max_frame_bytes,
};
pub use retention::{
    ARCHIVE_RETENTION_ENV, ARCHIVE_RETENTION_FLAG, ARCHIVE_RETENTION_FORCE_DISCARD_ENV,
    ARCHIVE_RETENTION_FORCE_DISCARD_FLAG, ARCHIVE_RETENTION_SUB_AGENT_ENV,
    ARCHIVE_RETENTION_SUB_AGENT_FLAG, ARCHIVE_RETENTION_SWEEP_INTERVAL, ArchiveRetentionSource,
    ArchiveRetentionSweepDriver, ArchiveSweepReport, ArchiveSweepSkip,
    apply_startup_archive_retention, parse_force_discard, parse_retention_window,
    resolve_archive_retention, resolve_archive_retention_with_sub_agent,
    run_archive_retention_sweeps, sweep_archive_retention_at_startup,
};
pub use serve::{
    ServingBackend, bind_exposure_warning, ensure_state_database, ensure_state_database_with,
    open_backend, start_remote_server, start_remote_server_with_participants,
};

const DEFAULT_EVENT_RETENTION: usize = 4096;
/// Most events one `SessionEvents` response carries, whatever its byte budget.
///
/// The batch is produced on the thread that answers the request, so a caller
/// resuming a long journal must not wait for every retained event at once and
/// the response must not grow with the whole backlog. The count bound applies to
/// every caller, including an in-process one, whose connection has no frame to
/// fit: such a caller still follows pages until the response names no
/// continuation cursor. The byte bound the same response honours is the frame
/// limit of the connection that sends it; see
/// [`InProcessConnection::with_max_response_bytes`].
const MAX_EVENT_BATCH_EVENTS: usize = 256;
const MAX_REVIEW_CHANGES: usize = 512;

const MAX_REVIEW_DIFF_BYTES: usize = 64 * 1024;
const MAX_REVIEW_FILE_BYTES: usize = 128 * 1024;
const MAX_RUN_MESSAGE_BYTES: usize = 32 * 1024;

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
struct EventJournal {
    next_sequence: EventSequence,
    /// Retained session events, indexed by session so append, retention
    /// pruning and range reads only touch one session's window instead of the
    /// whole journal. Each deque is ordered by ascending global sequence,
    /// which keeps the head as that session's oldest and the tail as its
    /// latest retained sequence.
    events: BTreeMap<AgentSessionId, VecDeque<ServerEventEnvelope>>,
    /// Unpersisted session events, kept in one globally ordered vector because
    /// the durable feed requires strictly increasing sequences.
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
    filesystem: loom_sessionfs::WorkspaceStateSnapshot,
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
    worker: Mutex<Option<run_executor::RunTask>>,
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
/// How long shutdown waits for each run to settle before proceeding without
/// joining it. A cancelled command terminates immediately; only a step that
/// cannot observe cancellation (for example a blocked HTTP read) lingers.
const SHUTDOWN_SETTLE_TIMEOUT: Duration = Duration::from_secs(5);
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
    session_service: SessionService,
    workspace_service: WorkspaceService,
    run_service: RunService,
    journal: Mutex<EventJournal>,
    last_feed_pruned_sequence: AtomicU64,
    feed_bytes_since_prune: AtomicUsize,
    session_filesystem_service: SessionFilesystemService,
    repository_service: RepositoryService,
    process_service: ProcessService,
    terminals: TerminalManager,
    resource_monitor: Mutex<ResourceMonitor>,
    supported_capabilities: CapabilitySet,
    providers: ProviderRegistry,
    credentials: CredentialService,
    persistence: Option<Arc<dyn Persistence>>,
    run_executor: RunExecutor,
    session_root_base: PathBuf,
    /// Per-instance root for session build output. Present for a durable
    /// backend, absent for the in-memory one.
    build_cache_base: Option<PathBuf>,
    /// Configured archive-retention policy; read through
    /// [`InProcessBackend::archive_retention`].
    archive_retention: Mutex<loom_protocol::ArchiveRetentionPolicy>,
    /// Whether the policy in effect is the one stored in durable state, either
    /// loaded when this backend opened or written by a later change; read
    /// through [`InProcessBackend::archive_retention_is_stored`].
    archive_retention_stored: AtomicBool,
    idempotency_store: IdempotencyStore,
    admissions: AdmissionService,
    /// Participant directory, session grants, and connection presence.
    access: Arc<AccessService>,
    /// Per-run ordering of people's directions and control actions.
    collaboration: CollaborationService,
    self_reference: Mutex<Weak<InProcessBackend>>,
    request_lifecycle: RwLock<u8>,
    persistence_failed: AtomicBool,
    state_persist_gate: Mutex<()>,
    /// Serializes session deletion with the retention sweep, so no caller ever
    /// observes a partially deleted session.
    archive_gate: Mutex<()>,
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
struct ListProjectChildrenArguments {
    /// Optional direct-child task selector. When given, only that child is
    /// listed and its stored result message is returned in full.
    #[serde(default)]
    task_id: Option<loom_core::TaskId>,
}

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
    /// Most bytes one response this connection sends may carry.
    ///
    /// The events batch is the one response that grows with journal size, so it
    /// is bounded to this many bytes. `None` means the connection has no frame
    /// transport to fit (an in-process caller), so the batch is bounded by event
    /// count alone. The remote listener sets it from
    /// [`RemoteServerConfig::max_frame_bytes`], because the server must not send
    /// a frame larger than the limit it advertises to its clients.
    max_response_bytes: Option<usize>,
    /// The live presence of the person this connection authenticated as.
    /// Present only for connections bound to a participant; dropping the last
    /// clone of the connection removes it from the roster.
    presence: Option<Arc<ConnectionPresence>>,
}

impl InProcessConnection {
    pub fn disconnected(backend: Arc<InProcessBackend>) -> Self {
        Self {
            backend,
            negotiated_capabilities: Arc::new(Mutex::new(None)),
            auth: None,
            max_response_bytes: None,
            presence: None,
        }
    }

    /// The person this connection is bound to, if any.
    pub fn participant(&self) -> Option<&loom_core::ParticipantIdentity> {
        self.auth.as_ref().and_then(AuthSession::participant)
    }

    /// Presence handle of this connection, so a transport can refresh
    /// liveness and observe revocation.
    pub fn presence(&self) -> Option<&Arc<ConnectionPresence>> {
        self.presence.as_ref()
    }

    /// Bounds every response this connection sends to `max_response_bytes`.
    ///
    /// A frame transport applies its own configured frame limit, so a response
    /// this worker sends never exceeds the bound its clients check against.
    pub(crate) fn with_max_response_bytes(mut self, max_response_bytes: usize) -> Self {
        self.max_response_bytes = Some(max_response_bytes);
        self
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
