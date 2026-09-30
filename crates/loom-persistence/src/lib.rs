use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::{Read, Write},
    ops::{Deref, DerefMut},
    path::{Path, PathBuf},
    sync::{Arc, Mutex, MutexGuard},
};

use flate2::{Compression, read::ZlibDecoder, write::ZlibEncoder};
use loom_core::{
    ActivityId, AgentMessageDraft, AgentMessageId, AgentMessageKind, AgentMessageRecord,
    AgentSessionId, AgentSessionSnapshot, AgentSessionState, ApprovalPolicy, CheckpointId,
    DelegatedTaskRecord, DelegatedTaskSpec, DelegatedTaskStatus, ErrorCode, EventSequence,
    InteractionId, LoomError, PolicyDecision, ProjectAgentPermissions, ProjectAgentRecord,
    ProjectId, ProjectManagerWaitId, ProjectManagerWaitRecord, ProjectManagerWaitStatus,
    ProjectSnapshot, ProjectWorktreeCleanupDisposition, ProjectWorktreeRecord,
    ProjectWorktreeStatus, RepositoryId, RequestId, Result, RunAttemptId, RunId, SessionLimits,
    SessionManagerState, StepId, TaskContextReference, TaskId, Timestamp, ToolCallId,
    UsageSnapshot, WorkspaceId, WorkspaceManagerState, WorkspaceRecord,
};
use loom_model::{
    ModelId, ProviderConfig, ProviderHealth, ProviderId, ProviderUsageKey, ProviderUsageSummary,
    UsageLedger,
};
use loom_protocol::{
    AgentActivityData, AgentActivityKind, AgentActivityRecord, AgentActivityStatus,
    AgentExecutionStateRecord, AgentInteractionKind, AgentInteractionRecord,
    AgentInteractionStatus, AgentPlan, AgentPlanStep, AgentRunAttemptRecord, AgentRunSnapshot,
    AgentRunState, AgentToolAttemptRecord, AgentToolAttemptState, AgentToolCallRecord,
    ApprovalDecision, Checkpoint, CheckpointFile, ContextAssemblyOptions, ContextInspection,
    ContextSummary, ServerEventEnvelope, SessionDirectory, SessionFilesystemChange,
    SessionRepository, ToolResult, WorkspaceChangeKind, WorkspaceConfig, WorkspaceControl,
    WorkspaceEventEnvelope, WorkspaceFeedEvent,
};
use rusqlite::{Connection, OptionalExtension, Transaction, params, types::Value as SqlValue};
use serde::{Serialize, de::DeserializeOwned};
use serde_json::Value;
use sha2::{Digest, Sha256};
use uuid::Uuid;

mod catalog;
mod codec;
mod content;
mod feed;
mod filesystem;
mod lifecycle;
mod persistence_trait;
mod project;
mod runs;
mod schema;

pub use persistence_trait::Persistence;

use catalog::*;
use codec::*;
use content::*;
use feed::*;
use filesystem::*;
use project::*;
use runs::*;
pub use schema::*;

/// Baseline schema version for the current typed model.
///
/// Loom is pre-1.0. Most schema changes are handled by wiping the state
/// database, and any upgrade step kept here is evaluated case by case and may
/// be dropped again before 1.0. When a step exists, [`migrate_schema`] upgrades
/// older supported versions in place; versions newer than this build, or with
/// no registered step, are rejected and must be wiped by the operator. New
/// optional state should prefer a versioned JSON payload column over a new
/// column where the payload column already exists.
const DATABASE_SCHEMA_VERSION: u32 = 4;
/// Oldest schema version that the migration ladder can upgrade in place.
const DATABASE_MIN_MIGRATABLE_VERSION: u32 = 2;
const EXTERNAL_STRING_THRESHOLD: usize = 4096;
const MAX_CONTENT_BYTES: usize = 512 * 1024 * 1024;
const INLINE_CONTENT_BYTES: usize = 4096;
const CONTENT_PART_BYTES: usize = 256 * 1024;
const MAX_TOOL_ARGUMENT_BYTES: usize = 1024 * 1024;
const MAX_MESSAGE_FRAGMENT_BYTES: usize = 32 * 1024;
const MAX_CONTENT_RANGE_BYTES: usize =
    loom_protocol::MAX_AGENT_RUN_MESSAGE_CONTENT_RANGE_BYTES as usize;
const MAX_RUN_MESSAGE_PAGE_SIZE: usize = loom_protocol::MAX_AGENT_RUN_MESSAGE_PAGE_SIZE as usize;
const MAX_FEED_EVENT_BYTES: usize = 128 * 1024 * 1024;
const MAX_DURABLE_FEED_SESSION_BYTES: usize = 16 * 1024 * 1024;
const MAX_DURABLE_FEED_TOTAL_BYTES: usize = 16 * 1024 * 1024;
const MAX_CONTENT_GC_CANDIDATES_PER_WRITE: usize = 256;
const MAX_MANUAL_CONTENT_GC_CANDIDATES: usize = 16_384;
const MAX_FILESYSTEM_CHANGE_HISTORY: usize = 2048;
const MAX_FILESYSTEM_CHANGE_PAGE_SIZE: usize = 512;
const MAX_IDEMPOTENCY_PAYLOAD_BYTES: usize = 1024 * 1024;
const MAX_RUN_RUNTIME_CONFIG_BYTES: usize = 1024 * 1024;
const MAX_PROJECT_MANAGER_WAIT_CHILDREN: usize = 50;
const MAX_PROJECT_MANAGER_WAIT_RESULT_SUMMARY_BYTES: usize = 64 * 1024;

#[derive(Clone, Debug)]
pub struct FilePersistence {
    path: PathBuf,
    connection: Arc<Mutex<Option<Connection>>>,
    owner_lock: Arc<Mutex<Option<fs::File>>>,
    in_memory: bool,
}

struct CachedConnection<'a>(MutexGuard<'a, Option<Connection>>);

impl Deref for CachedConnection<'_> {
    type Target = Connection;

    fn deref(&self) -> &Self::Target {
        self.0
            .as_ref()
            .expect("cached SQLite connection is initialized")
    }
}

impl DerefMut for CachedConnection<'_> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.0
            .as_mut()
            .expect("cached SQLite connection is initialized")
    }
}

#[derive(Clone, Debug)]
pub struct DurableFeedState {
    pub next_sequence: EventSequence,
    pub retention_limit: usize,
    pub events: Vec<ServerEventEnvelope>,
    pub workspace_events: Vec<WorkspaceEventEnvelope>,
}

#[derive(Clone, Copy, Debug)]
pub struct DurableFeedHeader {
    pub next_sequence: EventSequence,
    pub retention_limit: usize,
}

#[derive(Clone, Copy, Debug)]
pub struct DurableFeedSessionCursor {
    pub first_sequence: EventSequence,
    pub latest_sequence: EventSequence,
    pub pruned_through: EventSequence,
    pub oldest_retained_sequence: Option<EventSequence>,
}

#[derive(Clone, Copy, Debug)]
pub struct DurableFeedWorkspaceCursor {
    pub first_sequence: EventSequence,
    pub latest_sequence: EventSequence,
    pub pruned_through: EventSequence,
    pub oldest_retained_sequence: Option<EventSequence>,
}

#[derive(Clone, Debug, Default)]
pub struct DurableSessionSettings {
    pub approval_policies: BTreeMap<AgentSessionId, ApprovalPolicy>,
    pub auto_approve_actions: BTreeMap<AgentSessionId, bool>,
}

#[derive(Clone, Debug, Default)]
pub struct DurableProviderState {
    pub configs: BTreeMap<ProviderId, ProviderConfig>,
    pub health: BTreeMap<ProviderId, ProviderHealth>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct DurableIdempotencyRecord {
    pub created_at: Timestamp,
    pub expires_at: Option<Timestamp>,
    pub request: Value,
    pub response: Value,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DurableRunSummary {
    pub snapshot: AgentRunSnapshot,
    pub usage: UsageSnapshot,
    pub attempts: Option<Vec<AgentRunAttemptRecord>>,
    pub execution_state: Option<AgentExecutionStateRecord>,
    pub interactions: Option<Vec<AgentInteractionRecord>>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DurableRunRuntimeConfig {
    pub system_instructions: Option<String>,
    pub repository_instructions: Option<String>,
    pub approval_policy: ApprovalPolicy,
    pub limits: SessionLimits,
    pub context_options: ContextAssemblyOptions,
    pub checkpoint_id: Option<CheckpointId>,
    pub input_cost_micros_per_1k: u64,
    pub output_cost_micros_per_1k: u64,
    pub context_inspection: Option<ContextInspection>,
    pub project_delegation_enabled: bool,
    pub project_messaging_enabled: bool,
    pub project_inspection_enabled: bool,
    pub project_child_control_enabled: bool,
    pub project_worktree_enabled: bool,
    pub project_review_enabled: bool,
    pub project_integration_enabled: bool,
    pub project_branch_messaging_enabled: bool,
}

/// Versioned JSON payload persisted in `run_runtime_config.project_grants`.
///
/// This is the single storage representation for the per-run project-agent
/// grants. Unknown keys are ignored and missing keys default to `false`, so a
/// new grant is added here without a schema migration.
#[derive(Clone, Copy, Debug, Default, serde::Deserialize, Eq, PartialEq, serde::Serialize)]
#[serde(default)]
struct RunProjectGrants {
    delegation: bool,
    messaging: bool,
    inspection: bool,
    child_control: bool,
    worktree: bool,
    review: bool,
    integration: bool,
    branch_messaging: bool,
}

impl RunProjectGrants {
    fn of(config: &DurableRunRuntimeConfig) -> Self {
        Self {
            delegation: config.project_delegation_enabled,
            messaging: config.project_messaging_enabled,
            inspection: config.project_inspection_enabled,
            child_control: config.project_child_control_enabled,
            worktree: config.project_worktree_enabled,
            review: config.project_review_enabled,
            integration: config.project_integration_enabled,
            branch_messaging: config.project_branch_messaging_enabled,
        }
    }

    fn apply(self, config: &mut DurableRunRuntimeConfig) {
        config.project_delegation_enabled = self.delegation;
        config.project_messaging_enabled = self.messaging;
        config.project_inspection_enabled = self.inspection;
        config.project_child_control_enabled = self.child_control;
        config.project_worktree_enabled = self.worktree;
        config.project_review_enabled = self.review;
        config.project_integration_enabled = self.integration;
        config.project_branch_messaging_enabled = self.branch_messaging;
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DurableRunContextCheckpoint {
    pub session_id: AgentSessionId,
    pub summary: ContextSummary,
}

/// Durable intent for an in-progress deepest-first project cancellation.
/// `members` preserves the exact task/session snapshot captured before any
/// member was changed so restart recovery can safely replay the operation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProjectCancellationCascadeRecord {
    pub project_id: ProjectId,
    pub root_task_id: TaskId,
    pub manager_session_id: AgentSessionId,
    pub members: Vec<(TaskId, AgentSessionId)>,
    pub created_at: Timestamp,
}

/// Persisted inputs used to hydrate the latest run during a session bootstrap.
/// All fields are read from the same SQLite snapshot; process-local state and
/// journal events are intentionally handled by the server separately.
#[derive(Clone, Debug)]
pub struct DurableSessionProjectionRead {
    pub latest_run: Option<DurableRunSummary>,
    pub runtime_config: Option<DurableRunRuntimeConfig>,
    pub execution_state: Option<AgentExecutionStateRecord>,
    pub plan: AgentPlan,
    pub context_checkpoint: Option<DurableRunContextCheckpoint>,
    pub activities: Vec<AgentActivityRecord>,
    pub attempts: Vec<AgentRunAttemptRecord>,
    pub interactions: Vec<AgentInteractionRecord>,
    pub latest_sequence: Option<EventSequence>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DurableRunMessage {
    pub timeline_ordinal: u64,
    pub role: loom_model::MessageRole,
    pub content: String,
    pub name: Option<String>,
    pub tool_call_id: Option<loom_core::ToolCallId>,
    pub tool_calls: Vec<loom_model::ToolCall>,
    /// Provider reasoning that must be echoed back on assistant turns
    /// (for example DeepSeek thinking mode).
    pub reasoning_content: Option<String>,
}

/// A normal worker checkpoint writes only the mutable transcript tail. A retry
/// starts a new transcript generation and replaces the prior generation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DurableRunMessageDelta {
    pub start_ordinal: u64,
    pub reset: bool,
    pub messages: Vec<DurableRunMessage>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DurableRunMessageHeader {
    pub ordinal: u64,
    pub timeline_ordinal: u64,
    pub role: loom_model::MessageRole,
    pub content_bytes: u64,
    pub name: Option<String>,
    pub tool_call_id: Option<loom_core::ToolCallId>,
    pub tool_calls: Vec<loom_model::ToolCall>,
    /// Provider reasoning retained on the assistant turn for replay.
    pub reasoning_content: Option<String>,
}

pub type DurableRunActivities = BTreeMap<RunId, Vec<AgentActivityRecord>>;

/// A worker checkpoint contains only one run and its owning session filesystem.
/// Catalogs and idempotency records are persisted by the broader state flush.
pub struct DurableRunCheckpointWrite<'a> {
    pub session: &'a AgentSessionSnapshot,
    pub session_next_sequence: EventSequence,
    pub prune_feed: bool,
    pub summary: &'a DurableRunSummary,
    pub runtime_config: &'a DurableRunRuntimeConfig,
    pub context_checkpoint: Option<&'a DurableRunContextCheckpoint>,
    pub plan: &'a AgentPlan,
    pub messages: &'a [DurableRunMessage],
    /// Incremental transcript tail for worker checkpoints. `None` keeps the
    /// full replacement behavior used by explicit recovery/state flushes.
    pub message_delta: Option<&'a DurableRunMessageDelta>,
    pub activities: &'a [AgentActivityRecord],
    /// Activity rows changed since the previous successful worker checkpoint.
    /// When present, these are upserted without enumerating or pruning history.
    pub activity_deltas: Option<&'a [AgentActivityRecord]>,
    pub filesystem: Option<&'a DurableFilesystemRecord>,
    pub feed: &'a DurableFeedState,
}

#[derive(Clone, Debug)]
pub struct DurableFilesystemRecord {
    pub session_id: AgentSessionId,
    pub root: String,
    pub control: WorkspaceControl,
    pub checkpoints: Vec<Checkpoint>,
    pub edits: Vec<DurableFilesystemEdit>,
    pub changes: Vec<SessionFilesystemChange>,
    pub repositories: BTreeMap<RepositoryId, SessionRepository>,
    pub directories: Vec<SessionDirectory>,
    pub payload: Value,
    pub delta: Option<DurableFilesystemDelta>,
}

#[derive(Clone, Debug, Default)]
pub struct DurableFilesystemDelta {
    pub deleted_checkpoints: Vec<CheckpointId>,
    pub deleted_edits: Vec<u64>,
    pub deleted_changes: Vec<EventSequence>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DurableFilesystemChangesPage {
    pub changes: Vec<SessionFilesystemChange>,
    pub truncated: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DurableFilesystemEdit {
    pub id: u64,
    pub path: String,
    pub before: Option<String>,
    pub before_bytes: Option<Vec<u8>>,
    pub after_revision: String,
    pub source: WorkspaceControl,
}

pub struct DurableStateWrite<'a> {
    pub sessions: &'a SessionManagerState,
    pub workspaces: Option<&'a WorkspaceManagerState>,
    pub settings: Option<&'a DurableSessionSettings>,
    pub workspace_configs: Option<&'a BTreeMap<WorkspaceId, WorkspaceConfig>>,
    pub providers: Option<&'a DurableProviderState>,
    pub usage: Option<&'a UsageLedger>,
    pub idempotency: Option<&'a BTreeMap<RequestId, DurableIdempotencyRecord>>,
    pub run_summaries: Option<&'a BTreeMap<RunId, DurableRunSummary>>,
    pub run_runtime_configs: Option<&'a BTreeMap<RunId, DurableRunRuntimeConfig>>,
    pub run_context_checkpoints: Option<&'a BTreeMap<RunId, Option<DurableRunContextCheckpoint>>>,
    pub run_plans: Option<&'a BTreeMap<RunId, AgentPlan>>,
    pub run_messages: Option<&'a BTreeMap<RunId, Vec<DurableRunMessage>>>,
    pub run_activities: Option<&'a DurableRunActivities>,
    pub filesystem_records: Option<&'a [DurableFilesystemRecord]>,
    pub feed: Option<&'a DurableFeedState>,
}

/// Compatibility of an on-disk Loom state database with the current build.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SchemaStatus {
    /// No state database exists yet; opening creates the baseline schema.
    Absent,
    /// The database is at the current baseline schema version.
    Current,
    /// The database is at an older version that the migration ladder can
    /// upgrade in place when it is opened.
    Migratable(u32),
    /// The database was written at an unknown or newer schema version.
    OtherVersion(u32),
    /// The file exists but is not a recognizable SQLite database.
    Unrecognized,
}

impl SchemaStatus {
    /// Whether the database can be opened without wiping it.
    pub fn is_compatible(self) -> bool {
        matches!(self, Self::Absent | Self::Current | Self::Migratable(_))
    }

    /// Short description used in prompts and error messages.
    pub fn description(self) -> String {
        match self {
            Self::Absent => "no existing database".to_owned(),
            Self::Current => format!("schema version {DATABASE_SCHEMA_VERSION}"),
            Self::Migratable(version) => {
                format!("schema version {version} (will be upgraded on open)")
            }
            Self::OtherVersion(version) => format!("schema version {version}"),
            Self::Unrecognized => "an unrecognized format".to_owned(),
        }
    }
}

#[cfg(test)]
mod tests;
