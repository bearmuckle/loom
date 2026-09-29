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
mod content;
mod feed;
mod filesystem;
mod lifecycle;
mod project;
mod runs;
mod schema;

use content::*;
use feed::*;
use filesystem::*;
use project::*;
use runs::*;
use schema::*;

/// Baseline schema version for the current typed model.
///
/// Loom is pre-1.0 and deliberately has **no migration ladder**: this constant
/// is the only supported layout. A database written by any other Loom revision
/// is rejected and must be wiped by the operator. When the model changes, bump
/// this value and adjust [`DATABASE_SCHEMA`] (and the companion schema
/// fragments) in place; do not reintroduce incremental migrations or legacy
/// import paths. New optional state should prefer a versioned JSON payload
/// column over a new column that would need its own migration.
const DATABASE_SCHEMA_VERSION: u32 = 1;
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
///
/// Loom is pre-1.0 and has no migration ladder, so a database that is not at
/// the current baseline must be wiped by the operator before it can be opened.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SchemaStatus {
    /// No state database exists yet; opening creates the baseline schema.
    Absent,
    /// The database is at the current baseline schema version.
    Current,
    /// The database was written at a different schema version.
    OtherVersion(u32),
    /// The file exists but is not a recognizable SQLite database.
    Unrecognized,
}

impl SchemaStatus {
    /// Whether the database can be opened without wiping it.
    pub fn is_compatible(self) -> bool {
        matches!(self, Self::Absent | Self::Current)
    }

    /// Short description used in prompts and error messages.
    pub fn description(self) -> String {
        match self {
            Self::Absent => "no existing database".to_owned(),
            Self::Current => format!("schema version {DATABASE_SCHEMA_VERSION}"),
            Self::OtherVersion(version) => format!("schema version {version}"),
            Self::Unrecognized => "an unrecognized format".to_owned(),
        }
    }
}

/// Error raised when an incompatible database is opened without an explicit
/// wipe. It tells the operator how to reset state instead of failing silently.
pub fn incompatible_database_error(path: &Path, status: SchemaStatus) -> LoomError {
    LoomError::new(
        ErrorCode::MalformedPayload,
        format!(
            "persistence database '{}' uses {}; this build requires schema version \
             {DATABASE_SCHEMA_VERSION} and does not migrate or import existing state. \
             Re-run with state reset enabled (for example `--reset-state`) to wipe it, \
             or point Loom at a different state path.",
            path.display(),
            status.description()
        ),
        false,
    )
}

/// Inspects an on-disk database and, only when `wipe_if_incompatible` is true,
/// wipes it so a fresh baseline can be created. Never wipes implicitly.
pub fn prepare_database(path: &Path, wipe_if_incompatible: bool) -> Result<SchemaStatus> {
    let status = FilePersistence::schema_status(path)?;
    if status.is_compatible() || !wipe_if_incompatible {
        return Ok(status);
    }
    FilePersistence::reset_database(path)?;
    Ok(SchemaStatus::Absent)
}

/// The database file plus the SQLite sidecar files that must be removed
/// together for a clean wipe.
fn database_files(path: &Path) -> [PathBuf; 4] {
    let mut wal = path.as_os_str().to_os_string();
    wal.push("-wal");
    let mut shm = path.as_os_str().to_os_string();
    shm.push("-shm");
    let mut journal = path.as_os_str().to_os_string();
    journal.push("-journal");
    [
        path.to_path_buf(),
        PathBuf::from(wal),
        PathBuf::from(shm),
        PathBuf::from(journal),
    ]
}

fn map_schema_version(version: u32) -> SchemaStatus {
    if version == DATABASE_SCHEMA_VERSION {
        SchemaStatus::Current
    } else if version == 0 {
        // A SQLite file that has not been initialized yet; `initialize_schema`
        // decides whether it is empty or an unrecognized layout.
        SchemaStatus::Absent
    } else {
        SchemaStatus::OtherVersion(version)
    }
}

/// Reads `PRAGMA user_version` through a read-only connection. Returns `None`
/// when the file cannot be opened read-only or is not a SQLite database.
fn schema_status_via_read_only_sqlite(path: &Path) -> Option<SchemaStatus> {
    let connection =
        Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).ok()?;
    let version: u32 = connection
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .ok()?;
    Some(map_schema_version(version))
}

/// Reads the schema version from the SQLite file header without opening the
/// database. Used when a read-only SQLite connection is unavailable.
fn schema_status_from_header(path: &Path) -> Result<SchemaStatus> {
    let mut header = [0u8; 100];
    let mut file = fs::File::open(path).map_err(|error| {
        persistence_error(
            format!(
                "could not read persistence database '{}': {error}",
                path.display()
            ),
            true,
        )
    })?;
    match file.read_exact(&mut header) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof => {
            return Ok(SchemaStatus::Unrecognized);
        }
        Err(error) => {
            return Err(persistence_error(
                format!(
                    "could not read persistence database '{}': {error}",
                    path.display()
                ),
                true,
            ));
        }
    }
    if &header[..16] != b"SQLite format 3\0" {
        return Ok(SchemaStatus::Unrecognized);
    }
    let version = u32::from_be_bytes([header[60], header[61], header[62], header[63]]);
    Ok(map_schema_version(version))
}

fn initialize_schema(connection: &Connection) -> Result<()> {
    let database_version: u32 = connection
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .map_err(|error| {
            persistence_error(
                format!("could not inspect persistence schema: {error}"),
                true,
            )
        })?;
    if database_version == DATABASE_SCHEMA_VERSION {
        return Ok(());
    }

    // Loom is pre-1.0: there is no migration ladder and no legacy import. A
    // database that was not written at the current baseline is rejected
    // unchanged and must be wiped by the operator.
    if database_version != 0 {
        return Err(unsupported_database_error(database_version));
    }

    // Inspect before changing persistent SQLite settings. Unsupported databases
    // must not even have their journal mode changed; this release starts with a
    // new database only.
    let has_user_tables: bool = connection
        .query_row(
            "SELECT EXISTS(
                SELECT 1 FROM sqlite_master
                WHERE type = 'table' AND name NOT LIKE 'sqlite_%'
            )",
            [],
            |row| row.get(0),
        )
        .map_err(|error| {
            persistence_error(
                format!("could not inspect persistence database: {error}"),
                true,
            )
        })?;
    if has_user_tables {
        return Err(unsupported_database_error(0));
    }

    connection
        .execute_batch("PRAGMA journal_mode = WAL;")
        .map_err(|error| {
            persistence_error(
                format!("could not initialize persistence database: {error}"),
                true,
            )
        })?;

    let transaction = connection.unchecked_transaction().map_err(|error| {
        persistence_error(
            format!("could not initialize persistence schema: {error}"),
            true,
        )
    })?;
    transaction
        .execute_batch(DATABASE_SCHEMA)
        .map_err(|error| {
            persistence_error(
                format!("could not create persistence schema: {error}"),
                true,
            )
        })?;
    transaction
        .execute_batch(PROJECT_TASK_SCHEMA)
        .map_err(|error| {
            persistence_error(
                format!("could not create project task schema: {error}"),
                true,
            )
        })?;
    transaction
        .execute_batch(PROJECT_WORKTREE_SCHEMA)
        .map_err(|error| {
            persistence_error(
                format!("could not create project worktree schema: {error}"),
                true,
            )
        })?;
    transaction
        .execute_batch(PROJECT_MANAGER_WAIT_SCHEMA)
        .map_err(|error| {
            persistence_error(
                format!("could not create project manager wait schema: {error}"),
                true,
            )
        })?;
    transaction
        .execute_batch(PROJECT_CANCELLATION_CASCADE_SCHEMA)
        .map_err(|error| {
            persistence_error(
                format!("could not create project cancellation recovery schema: {error}"),
                true,
            )
        })?;
    transaction
        .pragma_update(None, "user_version", DATABASE_SCHEMA_VERSION)
        .map_err(|error| {
            persistence_error(
                format!("could not record persistence schema version: {error}"),
                true,
            )
        })?;
    transaction.commit().map_err(|error| {
        persistence_error(
            format!("could not commit persistence schema: {error}"),
            true,
        )
    })?;
    Ok(())
}

fn unsupported_database_error(database_version: u32) -> LoomError {
    let found = if database_version == 0 {
        "an unrecognized format".to_owned()
    } else {
        format!("schema version {database_version}")
    };
    LoomError::new(
        ErrorCode::MalformedPayload,
        format!(
            "persistence database uses {found}; this build requires schema version {DATABASE_SCHEMA_VERSION} and does not migrate or import existing state. Wipe the state database to start fresh."
        ),
        false,
    )
}

fn store_content(transaction: &Transaction<'_>, content: &[u8]) -> Result<Vec<u8>> {
    if content.len() > MAX_CONTENT_BYTES {
        return Err(LoomError::invalid_request(
            "persisted content exceeds the maximum supported size",
        ));
    }
    let hash = Sha256::digest(content).to_vec();
    let already_stored = transaction
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM content_objects WHERE hash=?1)",
            [hash.as_slice()],
            |row| row.get::<_, bool>(0),
        )
        .map_err(|error| {
            persistence_error(format!("could not check state content: {error}"), true)
        })?;
    if already_stored {
        return Ok(hash);
    }
    let inline = (content.len() <= INLINE_CONTENT_BYTES)
        .then(|| encode_inline_content(content))
        .transpose()?;
    transaction
        .execute(
            "INSERT INTO content_objects(hash, raw_size, inline_codec, inline_payload)
             VALUES (?1, ?2, ?3, ?4)",
            params![
                hash,
                content.len() as i64,
                inline.as_ref().map(|(codec, _)| *codec),
                inline.as_ref().map(|(_, payload)| payload.as_slice()),
            ],
        )
        .map_err(|error| {
            persistence_error(format!("could not store content metadata: {error}"), true)
        })?;
    if inline.is_some() {
        return Ok(hash);
    }
    for (ordinal, part) in content.chunks(CONTENT_PART_BYTES).enumerate() {
        let byte_offset = ordinal * CONTENT_PART_BYTES;
        let part_hash = store_content_blob(transaction, part)?;
        transaction
            .execute(
                "INSERT INTO content_parts(
                    content_hash, ordinal, byte_offset, byte_length, blob_hash
                 ) VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    hash,
                    ordinal as i64,
                    byte_offset as i64,
                    part.len() as i64,
                    part_hash
                ],
            )
            .map_err(|error| {
                persistence_error(
                    format!("could not store content part reference: {error}"),
                    true,
                )
            })?;
    }
    Ok(hash)
}

fn encode_inline_content(content: &[u8]) -> Result<(i64, Vec<u8>)> {
    if content.len() >= 512 {
        let mut encoder = ZlibEncoder::new(Vec::new(), Compression::fast());
        encoder.write_all(content).map_err(|error| {
            persistence_error(
                format!("could not compress inline state content: {error}"),
                false,
            )
        })?;
        let compressed = encoder.finish().map_err(|error| {
            persistence_error(
                format!("could not compress inline state content: {error}"),
                false,
            )
        })?;
        if compressed.len().saturating_mul(100) <= content.len().saturating_mul(90) {
            return Ok((1, compressed));
        }
    }
    Ok((0, content.to_vec()))
}

fn store_content_blob(transaction: &Transaction<'_>, content: &[u8]) -> Result<Vec<u8>> {
    debug_assert!(!content.is_empty() && content.len() <= CONTENT_PART_BYTES);
    let hash = Sha256::digest(content).to_vec();
    let (codec, payload) = if content.len() >= EXTERNAL_STRING_THRESHOLD {
        let mut encoder = ZlibEncoder::new(Vec::new(), Compression::fast());
        encoder.write_all(content).map_err(|error| {
            persistence_error(format!("could not compress state content: {error}"), false)
        })?;
        let compressed = encoder.finish().map_err(|error| {
            persistence_error(format!("could not compress state content: {error}"), false)
        })?;
        if compressed.len().saturating_mul(100) <= content.len().saturating_mul(90) {
            (1_i64, compressed)
        } else {
            (0_i64, content.to_vec())
        }
    } else {
        (0_i64, content.to_vec())
    };
    transaction
        .execute(
            "INSERT INTO content_blobs(hash, raw_size, codec, payload) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(hash) DO NOTHING",
            params![hash, content.len() as i64, codec, payload],
        )
        .map_err(|error| {
            persistence_error(format!("could not store state content part: {error}"), true)
        })?;
    Ok(hash)
}

fn collect_unused_content(transaction: &Transaction<'_>, candidate_limit: usize) -> Result<()> {
    let candidate_limit = i64::try_from(candidate_limit).map_err(|_| {
        LoomError::invalid_request("content garbage-collection limit is out of range")
    })?;
    transaction
        .execute(
            "DELETE FROM content_objects
             WHERE hash IN (
                 SELECT hash FROM content_gc_candidates
                 WHERE kind='object' ORDER BY hash LIMIT ?1
             ) AND NOT EXISTS (
                SELECT 1 FROM checkpoint_files
                WHERE checkpoint_files.content_hash=content_objects.hash
             ) AND NOT EXISTS (
                SELECT 1 FROM run_messages
                WHERE run_messages.content_hash=content_objects.hash
             ) AND NOT EXISTS (
                SELECT 1 FROM run_context_checkpoints
                WHERE run_context_checkpoints.summary_hash=content_objects.hash
             ) AND NOT EXISTS (
                SELECT 1 FROM run_activities
                WHERE run_activities.data_hash=content_objects.hash
             ) AND NOT EXISTS (
                SELECT 1 FROM run_tool_calls
                WHERE run_tool_calls.arguments_hash=content_objects.hash
             ) AND NOT EXISTS (
                SELECT 1 FROM runtime_configurations
                WHERE runtime_configurations.system_instructions_hash=content_objects.hash
                   OR runtime_configurations.repository_instructions_hash=content_objects.hash
             ) AND NOT EXISTS (
                SELECT 1 FROM filesystem_edits
                WHERE filesystem_edits.before_hash=content_objects.hash
             )",
            [candidate_limit],
        )
        .map_err(|error| {
            persistence_error(
                format!("could not collect unused content metadata: {error}"),
                true,
            )
        })?;
    transaction
        .execute(
            "DELETE FROM content_gc_candidates
             WHERE kind='object' AND hash IN (
                 SELECT hash FROM content_gc_candidates
                 WHERE kind='object' ORDER BY hash LIMIT ?1
             )",
            [candidate_limit],
        )
        .map_err(|error| {
            persistence_error(
                format!("could not prune processed content candidates: {error}"),
                true,
            )
        })?;
    transaction
        .execute(
            "DELETE FROM content_blobs
             WHERE hash IN (
                 SELECT hash FROM content_gc_candidates
                 WHERE kind='blob' ORDER BY hash LIMIT ?1
             ) AND NOT EXISTS (
                 SELECT 1 FROM content_parts WHERE content_parts.blob_hash=content_blobs.hash
             )",
            [candidate_limit],
        )
        .map_err(|error| {
            persistence_error(
                format!("could not collect unused content parts: {error}"),
                true,
            )
        })?;
    transaction
        .execute(
            "DELETE FROM content_gc_candidates
             WHERE kind='blob' AND hash IN (
                 SELECT hash FROM content_gc_candidates
                 WHERE kind='blob' ORDER BY hash LIMIT ?1
             )",
            [candidate_limit],
        )
        .map_err(|error| {
            persistence_error(
                format!("could not prune processed blob candidates: {error}"),
                true,
            )
        })?;
    Ok(())
}

fn message_role_name(role: loom_model::MessageRole) -> &'static str {
    match role {
        loom_model::MessageRole::System => "system",
        loom_model::MessageRole::User => "user",
        loom_model::MessageRole::Assistant => "assistant",
        loom_model::MessageRole::Tool => "tool",
    }
}

fn parse_message_role(role: &str) -> Result<loom_model::MessageRole> {
    match role {
        "system" => Ok(loom_model::MessageRole::System),
        "user" => Ok(loom_model::MessageRole::User),
        "assistant" => Ok(loom_model::MessageRole::Assistant),
        "tool" => Ok(loom_model::MessageRole::Tool),
        _ => Err(LoomError::new(
            ErrorCode::MalformedPayload,
            "persisted message has an unknown role",
            false,
        )),
    }
}

fn activity_kind_name(kind: AgentActivityKind) -> &'static str {
    match kind {
        AgentActivityKind::ModelTurn => "model_turn",
        AgentActivityKind::ToolCall => "tool_call",
        AgentActivityKind::File => "file",
        AgentActivityKind::Search => "search",
        AgentActivityKind::Command => "command",
    }
}

fn parse_activity_kind(kind: &str) -> Result<AgentActivityKind> {
    match kind {
        "model_turn" => Ok(AgentActivityKind::ModelTurn),
        "tool_call" => Ok(AgentActivityKind::ToolCall),
        "file" => Ok(AgentActivityKind::File),
        "search" => Ok(AgentActivityKind::Search),
        "command" => Ok(AgentActivityKind::Command),
        _ => Err(LoomError::new(
            ErrorCode::MalformedPayload,
            "persisted activity has an unknown kind",
            false,
        )),
    }
}

fn activity_status_name(status: AgentActivityStatus) -> &'static str {
    match status {
        AgentActivityStatus::Started => "started",
        AgentActivityStatus::Completed => "completed",
        AgentActivityStatus::Failed => "failed",
        AgentActivityStatus::AwaitingApproval => "awaiting_approval",
        AgentActivityStatus::AwaitingInput => "awaiting_input",
        AgentActivityStatus::Cancelled => "cancelled",
    }
}

fn tool_attempt_state_name(state: AgentToolAttemptState) -> &'static str {
    match state {
        AgentToolAttemptState::Queued => "queued",
        AgentToolAttemptState::Running => "running",
        AgentToolAttemptState::AwaitingApproval => "awaiting_approval",
        AgentToolAttemptState::AwaitingInput => "awaiting_input",
        AgentToolAttemptState::Completed => "completed",
        AgentToolAttemptState::Failed => "failed",
        AgentToolAttemptState::Cancelled => "cancelled",
        AgentToolAttemptState::OutcomeUnknown => "outcome_unknown",
    }
}

fn parse_tool_attempt_state(state: &str) -> Result<AgentToolAttemptState> {
    match state {
        "queued" => Ok(AgentToolAttemptState::Queued),
        "running" => Ok(AgentToolAttemptState::Running),
        "awaiting_approval" => Ok(AgentToolAttemptState::AwaitingApproval),
        "awaiting_input" => Ok(AgentToolAttemptState::AwaitingInput),
        "completed" => Ok(AgentToolAttemptState::Completed),
        "failed" => Ok(AgentToolAttemptState::Failed),
        "cancelled" => Ok(AgentToolAttemptState::Cancelled),
        "outcome_unknown" => Ok(AgentToolAttemptState::OutcomeUnknown),
        _ => Err(LoomError::new(
            ErrorCode::MalformedPayload,
            format!("persisted tool-attempt state '{state}' is invalid"),
            false,
        )),
    }
}

fn parse_interaction_kind(kind: &str) -> Result<AgentInteractionKind> {
    match kind {
        "tool_approval" => Ok(AgentInteractionKind::ToolApproval),
        "user_input" => Ok(AgentInteractionKind::UserInput),
        _ => Err(LoomError::new(
            ErrorCode::MalformedPayload,
            format!("persisted interaction kind '{kind}' is invalid"),
            false,
        )),
    }
}

fn parse_interaction_status(status: &str) -> Result<AgentInteractionStatus> {
    match status {
        "pending" => Ok(AgentInteractionStatus::Pending),
        "approved" => Ok(AgentInteractionStatus::Approved),
        "rejected" => Ok(AgentInteractionStatus::Rejected),
        "answered" => Ok(AgentInteractionStatus::Answered),
        "abandoned" => Ok(AgentInteractionStatus::Abandoned),
        _ => Err(LoomError::new(
            ErrorCode::MalformedPayload,
            format!("persisted interaction status '{status}' is invalid"),
            false,
        )),
    }
}

fn parse_approval_decision(decision: &str) -> Result<ApprovalDecision> {
    match decision {
        "approved" => Ok(ApprovalDecision::Approved),
        "rejected" => Ok(ApprovalDecision::Rejected),
        _ => Err(LoomError::new(
            ErrorCode::MalformedPayload,
            format!("persisted approval decision '{decision}' is invalid"),
            false,
        )),
    }
}

fn interaction_kind_name(kind: AgentInteractionKind) -> &'static str {
    match kind {
        AgentInteractionKind::ToolApproval => "tool_approval",
        AgentInteractionKind::UserInput => "user_input",
    }
}

fn interaction_status_name(status: AgentInteractionStatus) -> &'static str {
    match status {
        AgentInteractionStatus::Pending => "pending",
        AgentInteractionStatus::Approved => "approved",
        AgentInteractionStatus::Rejected => "rejected",
        AgentInteractionStatus::Answered => "answered",
        AgentInteractionStatus::Abandoned => "abandoned",
    }
}

fn approval_decision_name(decision: ApprovalDecision) -> &'static str {
    match decision {
        ApprovalDecision::Approved => "approved",
        ApprovalDecision::Rejected => "rejected",
    }
}

fn parse_activity_status(status: &str) -> Result<AgentActivityStatus> {
    match status {
        "started" => Ok(AgentActivityStatus::Started),
        "completed" => Ok(AgentActivityStatus::Completed),
        "failed" => Ok(AgentActivityStatus::Failed),
        "awaiting_approval" => Ok(AgentActivityStatus::AwaitingApproval),
        "awaiting_input" => Ok(AgentActivityStatus::AwaitingInput),
        "cancelled" => Ok(AgentActivityStatus::Cancelled),
        _ => Err(LoomError::new(
            ErrorCode::MalformedPayload,
            "persisted activity has an unknown status",
            false,
        )),
    }
}

fn activity_data_tool_call_id(data: &AgentActivityData) -> Option<loom_core::ToolCallId> {
    match data {
        AgentActivityData::ModelTurn { .. } => None,
        AgentActivityData::ToolCall { call, .. }
        | AgentActivityData::File { call, .. }
        | AgentActivityData::Search { call, .. }
        | AgentActivityData::Command { call, .. } => Some(call.id),
    }
}

fn activity_data_kind(data: &AgentActivityData) -> AgentActivityKind {
    match data {
        AgentActivityData::ModelTurn { .. } => AgentActivityKind::ModelTurn,
        AgentActivityData::ToolCall { .. } => AgentActivityKind::ToolCall,
        AgentActivityData::File { .. } => AgentActivityKind::File,
        AgentActivityData::Search { .. } => AgentActivityKind::Search,
        AgentActivityData::Command { .. } => AgentActivityKind::Command,
    }
}

fn decode_optional_tool_call_id(id: Option<Vec<u8>>) -> Result<Option<loom_core::ToolCallId>> {
    id.map(|id| {
        Uuid::from_slice(&id)
            .map(loom_core::ToolCallId::from_uuid)
            .map_err(|error| {
                LoomError::new(
                    ErrorCode::MalformedPayload,
                    format!("persisted tool call id is malformed: {error}"),
                    false,
                )
            })
    })
    .transpose()
}

fn decode_optional_activity_id(id: Option<Vec<u8>>) -> Result<Option<ActivityId>> {
    id.map(|id| decode_uuid(&id, "parent activity id").map(ActivityId::from_uuid))
        .transpose()
}

fn decode_optional_step_id(id: Option<Vec<u8>>) -> Result<Option<StepId>> {
    id.map(|id| decode_uuid(&id, "activity step id").map(StepId::from_uuid))
        .transpose()
}

/// One streamed message fragment descriptor stored in `run_messages.fragments`.
#[derive(Clone, Debug, serde::Deserialize, serde::Serialize)]
struct StoredFragment {
    fragment_ordinal: u64,
    byte_offset: u64,
    byte_length: u64,
    content_hash: String,
}

/// One tool execution attempt stored in `run_tool_calls.attempts`.
#[derive(Clone, Debug, serde::Deserialize, serde::Serialize)]
struct StoredToolAttempt {
    activity_id: String,
    attempt_number: u32,
    state: String,
    started_at: i64,
    completed_at: Option<i64>,
    result: Option<ToolResult>,
}

fn decode_stored_attempts(payload: &str) -> Result<Vec<StoredToolAttempt>> {
    serde_json::from_str(payload).map_err(|error| {
        LoomError::new(
            ErrorCode::MalformedPayload,
            format!("persisted tool attempts are malformed: {error}"),
            false,
        )
    })
}

/// Inserts or updates one execution attempt on its logical tool call.
#[allow(clippy::too_many_arguments)]
fn upsert_stored_attempt(
    transaction: &Transaction<'_>,
    run_id_bytes: &[u8],
    call_id: ToolCallId,
    activity_id: ActivityId,
    state: AgentToolAttemptState,
    started_at: i64,
    completed_at: Option<i64>,
    result: Option<ToolResult>,
) -> Result<()> {
    let payload: Option<String> = transaction
        .query_row(
            "SELECT attempts FROM run_tool_calls WHERE run_id=?1 AND tool_call_id=?2",
            params![run_id_bytes, call_id.as_uuid().as_bytes().as_slice()],
            |row| row.get(0),
        )
        .optional()
        .map_err(|error| {
            persistence_error(format!("could not read tool attempts: {error}"), true)
        })?;
    let Some(payload) = payload else {
        return Err(LoomError::new(
            ErrorCode::MalformedPayload,
            "tool attempt has no logical tool-call row",
            false,
        ));
    };
    let mut attempts = decode_stored_attempts(&payload)?;
    let activity_hex = encode_hash_hex(activity_id.as_uuid().as_bytes());
    if let Some(slot) = attempts
        .iter_mut()
        .find(|attempt| attempt.activity_id == activity_hex)
    {
        slot.state = tool_attempt_state_name(state).to_owned();
        slot.started_at = started_at;
        slot.completed_at = completed_at;
        slot.result = result;
    } else {
        let attempt_number = attempts
            .iter()
            .map(|attempt| attempt.attempt_number)
            .max()
            .unwrap_or(0)
            .checked_add(1)
            .ok_or_else(|| {
                LoomError::new(
                    ErrorCode::Persistence,
                    "tool call has too many execution attempts",
                    false,
                )
            })?;
        attempts.push(StoredToolAttempt {
            activity_id: activity_hex,
            attempt_number,
            state: tool_attempt_state_name(state).to_owned(),
            started_at,
            completed_at,
            result,
        });
        attempts.sort_by_key(|attempt| attempt.attempt_number);
    }
    let payload = serde_json::to_string(&attempts).map_err(|error| {
        persistence_error(format!("could not encode tool attempts: {error}"), false)
    })?;
    transaction
        .execute(
            "UPDATE run_tool_calls SET attempts=?3 WHERE run_id=?1 AND tool_call_id=?2",
            params![
                run_id_bytes,
                call_id.as_uuid().as_bytes().as_slice(),
                payload
            ],
        )
        .map_err(|error| {
            persistence_error(format!("could not save tool attempt: {error}"), true)
        })?;
    Ok(())
}

fn encode_hash_hex(hash: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(hash.len() * 2);
    for byte in hash {
        out.push(DIGITS[(byte >> 4) as usize] as char);
        out.push(DIGITS[(byte & 0x0f) as usize] as char);
    }
    out
}

fn hex_nibble(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn decode_hash_hex(value: &str, field: &str) -> Result<Vec<u8>> {
    let bytes = value.as_bytes();
    if !bytes.len().is_multiple_of(2) {
        return Err(LoomError::new(
            ErrorCode::MalformedPayload,
            format!("persisted {field} is not a valid hex hash"),
            false,
        ));
    }
    let mut out = Vec::with_capacity(bytes.len() / 2);
    let mut index = 0;
    while index < bytes.len() {
        let (hi, lo) = match (hex_nibble(bytes[index]), hex_nibble(bytes[index + 1])) {
            (Some(hi), Some(lo)) => (hi, lo),
            _ => {
                return Err(LoomError::new(
                    ErrorCode::MalformedPayload,
                    format!("persisted {field} is not a valid hex hash"),
                    false,
                ));
            }
        };
        out.push((hi << 4) | lo);
        index += 2;
    }
    Ok(out)
}

fn decode_stored_fragments(payload: &str) -> Result<Vec<StoredFragment>> {
    serde_json::from_str(payload).map_err(|error| {
        LoomError::new(
            ErrorCode::MalformedPayload,
            format!("persisted message fragments are malformed: {error}"),
            false,
        )
    })
}

fn fragment_total(fragments: &[StoredFragment]) -> Option<u64> {
    fragments
        .iter()
        .filter_map(|fragment| fragment.byte_offset.checked_add(fragment.byte_length))
        .max()
}

fn load_run_message_fragments(
    connection: &Connection,
    run_id: RunId,
    message_ordinal: u64,
    byte_offset: u64,
    length: usize,
) -> Result<Vec<u8>> {
    let message_ordinal = i64::try_from(message_ordinal).map_err(|_| {
        LoomError::new(
            ErrorCode::MalformedPayload,
            "persisted message ordinal is out of range",
            false,
        )
    })?;
    let payload: Option<String> = connection
        .query_row(
            "SELECT fragments FROM run_messages WHERE run_id=?1 AND ordinal=?2",
            params![run_id.as_uuid().as_bytes().as_slice(), message_ordinal],
            |row| row.get(0),
        )
        .optional()
        .map_err(|error| {
            persistence_error(format!("could not read message fragments: {error}"), true)
        })?;
    let Some(payload) = payload else {
        return Ok(Vec::new());
    };
    let mut fragments = decode_stored_fragments(&payload)?;
    fragments.sort_by_key(|fragment| fragment.fragment_ordinal);
    let Some(total) = fragment_total(&fragments) else {
        return Ok(Vec::new());
    };
    if byte_offset >= total {
        return Ok(Vec::new());
    }
    let requested_length = u64::try_from(length)
        .map_err(|_| LoomError::invalid_request("message content range length is out of range"))?;
    let end = byte_offset.saturating_add(requested_length).min(total);
    let capacity = usize::try_from(end.saturating_sub(byte_offset)).map_err(|_| {
        LoomError::new(
            ErrorCode::MalformedPayload,
            "persisted message content range is too large",
            false,
        )
    })?;
    let mut output = Vec::with_capacity(capacity);
    if end == byte_offset {
        return Ok(output);
    }
    let mut cursor = byte_offset;
    for fragment in &fragments {
        let fragment_offset = fragment.byte_offset;
        let fragment_end = fragment
            .byte_offset
            .checked_add(fragment.byte_length)
            .ok_or_else(|| {
                LoomError::new(
                    ErrorCode::MalformedPayload,
                    "persisted message fragment range overflows",
                    false,
                )
            })?;
        if fragment_end <= cursor {
            continue;
        }
        if fragment_offset > cursor {
            return Err(LoomError::new(
                ErrorCode::MalformedPayload,
                "persisted message fragments contain a gap or invalid length",
                false,
            ));
        }
        let fragment_length = usize::try_from(fragment.byte_length).map_err(|_| {
            LoomError::new(
                ErrorCode::MalformedPayload,
                "persisted fragment length is invalid",
                false,
            )
        })?;
        let hash = decode_hash_hex(&fragment.content_hash, "fragment content hash")?;
        let bytes = decode_content(connection, &hash)?.into_bytes();
        if bytes.len() != fragment_length {
            return Err(LoomError::new(
                ErrorCode::MalformedPayload,
                "persisted message fragments contain a gap or invalid length",
                false,
            ));
        }
        let copy_start = cursor.max(fragment_offset);
        let copy_end = end.min(fragment_end);
        let local_start = usize::try_from(copy_start - fragment_offset).map_err(|_| {
            LoomError::new(
                ErrorCode::MalformedPayload,
                "persisted fragment range is out of bounds",
                false,
            )
        })?;
        let local_end = usize::try_from(copy_end - fragment_offset).map_err(|_| {
            LoomError::new(
                ErrorCode::MalformedPayload,
                "persisted fragment range is out of bounds",
                false,
            )
        })?;
        output.extend_from_slice(&bytes[local_start..local_end]);
        cursor = copy_end;
        if cursor == end {
            break;
        }
    }
    if cursor != end {
        return Err(LoomError::new(
            ErrorCode::MalformedPayload,
            "persisted message fragments do not cover the requested byte range",
            false,
        ));
    }
    Ok(output)
}

fn run_message_fragments_match(
    transaction: &Transaction<'_>,
    run_id: RunId,
    message_ordinal: i64,
    canonical_content: &[u8],
) -> Result<Option<bool>> {
    let payload: Option<String> = transaction
        .query_row(
            "SELECT fragments FROM run_messages WHERE run_id=?1 AND ordinal=?2",
            params![run_id.as_uuid().as_bytes().as_slice(), message_ordinal],
            |row| row.get(0),
        )
        .optional()
        .map_err(|error| {
            persistence_error(
                format!("could not read message fragments for consolidation: {error}"),
                true,
            )
        })?;
    let Some(payload) = payload else {
        return Ok(None);
    };
    let mut fragments = decode_stored_fragments(&payload)?;
    if fragments.is_empty() {
        return Ok(None);
    }
    fragments.sort_by_key(|fragment| fragment.fragment_ordinal);
    for fragment in &fragments {
        let byte_offset = usize::try_from(fragment.byte_offset).map_err(|_| {
            LoomError::new(
                ErrorCode::MalformedPayload,
                "persisted fragment offset is out of range",
                false,
            )
        })?;
        let hash = decode_hash_hex(&fragment.content_hash, "fragment content hash")?;
        let bytes = decode_content(transaction, &hash)?.into_bytes();
        let Some(fragment_end) = byte_offset.checked_add(bytes.len()) else {
            return Err(LoomError::new(
                ErrorCode::MalformedPayload,
                "persisted fragment range overflows",
                false,
            ));
        };
        if canonical_content.get(byte_offset..fragment_end) != Some(bytes.as_slice()) {
            return Ok(Some(false));
        }
    }
    Ok(Some(true))
}

fn delete_run_message_fragments(
    transaction: &Transaction<'_>,
    run_id: RunId,
    message_ordinal: i64,
) -> Result<()> {
    transaction
        .execute(
            "UPDATE run_messages SET fragments='[]'
             WHERE run_id=?1 AND ordinal=?2",
            params![run_id.as_uuid().as_bytes().as_slice(), message_ordinal],
        )
        .map_err(|error| {
            persistence_error(
                format!("could not consolidate run message fragments: {error}"),
                true,
            )
        })?;
    Ok(())
}

fn save_run_message_rows(
    transaction: &Transaction<'_>,
    messages_by_run: &BTreeMap<RunId, Vec<DurableRunMessage>>,
    ordinal_offsets: Option<&BTreeMap<RunId, u64>>,
    prune_missing: bool,
) -> Result<()> {
    transaction
        .execute_batch(
            "CREATE TEMP TABLE IF NOT EXISTS _loom_wanted_run_messages (
            run_id BLOB NOT NULL, ordinal INTEGER NOT NULL,
            PRIMARY KEY(run_id, ordinal)
         ) WITHOUT ROWID, STRICT;
         DELETE FROM _loom_wanted_run_messages;",
        )
        .map_err(|error| {
            persistence_error(format!("could not stage run messages: {error}"), true)
        })?;
    for (run_id, messages) in messages_by_run {
        let session_id: Vec<u8> = transaction
            .query_row(
                "SELECT session_id FROM run_summaries WHERE run_id=?1",
                [run_id.as_uuid().as_bytes().as_slice()],
                |row| row.get(0),
            )
            .map_err(|error| {
                persistence_error(
                    format!("run {run_id} has no durable summary for its transcript: {error}"),
                    true,
                )
            })?;
        for (ordinal, message) in messages.iter().enumerate() {
            let run_id_bytes = run_id.as_uuid().as_bytes();
            let ordinal = ordinal_offsets
                .and_then(|offsets| offsets.get(run_id))
                .copied()
                .unwrap_or_default()
                .checked_add(u64::try_from(ordinal).map_err(|_| {
                    LoomError::new(
                        ErrorCode::Persistence,
                        "run transcript has too many messages",
                        false,
                    )
                })?)
                .ok_or_else(|| {
                    LoomError::new(
                        ErrorCode::Persistence,
                        "run transcript ordinal overflow",
                        false,
                    )
                })?;
            let ordinal = i64::try_from(ordinal).map_err(|_| {
                LoomError::new(
                    ErrorCode::Persistence,
                    "run transcript has too many messages",
                    false,
                )
            })?;
            let timeline_ordinal = i64::try_from(message.timeline_ordinal).map_err(|_| {
                LoomError::new(
                    ErrorCode::Persistence,
                    "run timeline ordinal exceeds SQLite's integer range",
                    false,
                )
            })?;
            let fragments_match = run_message_fragments_match(
                transaction,
                *run_id,
                ordinal,
                message.content.as_bytes(),
            )?;
            let content_hash = match fragments_match {
                Some(false) => {
                    let existing: Option<Option<Vec<u8>>> = transaction
                        .query_row(
                            "SELECT content_hash FROM run_messages
                             WHERE run_id=?1 AND ordinal=?2",
                            params![run_id_bytes.as_slice(), ordinal],
                            |row| row.get(0),
                        )
                        .optional()
                        .map_err(|error| {
                            persistence_error(
                                format!("could not preserve fragmented message base: {error}"),
                                true,
                            )
                        })?;
                    existing.ok_or_else(|| {
                        LoomError::new(
                            ErrorCode::MalformedPayload,
                            "fragmented message has no base record",
                            false,
                        )
                    })?
                }
                Some(true) | None => {
                    if message.content.is_empty() {
                        None
                    } else {
                        Some(store_content(transaction, message.content.as_bytes())?)
                    }
                }
            };
            let tool_calls = serde_json::to_string(&message.tool_calls).map_err(|error| {
                persistence_error(
                    format!("could not encode message tool calls: {error}"),
                    false,
                )
            })?;
            if tool_calls.len() > 16 * 1024 * 1024 {
                return Err(LoomError::invalid_request(
                    "run message tool calls exceed the maximum supported size",
                ));
            }
            for call in &message.tool_calls {
                if call.name.len() > 4096 {
                    return Err(LoomError::invalid_request(
                        "run message tool name exceeds the maximum supported size",
                    ));
                }
                let arguments = serde_json::to_vec(&call.arguments).map_err(|error| {
                    persistence_error(format!("could not encode tool arguments: {error}"), false)
                })?;
                if arguments.len() > MAX_TOOL_ARGUMENT_BYTES {
                    return Err(LoomError::invalid_request(
                        "run message tool arguments exceed the maximum supported size",
                    ));
                }
            }
            transaction
                .execute(
                    "INSERT INTO _loom_wanted_run_messages(run_id, ordinal) VALUES (?1, ?2)",
                    params![run_id_bytes.as_slice(), ordinal],
                )
                .map_err(|error| {
                    persistence_error(format!("could not stage run message: {error}"), true)
                })?;
            let tool_call_id = message
                .tool_call_id
                .map(|id| id.as_uuid().as_bytes().to_vec());
            transaction
                .execute(
                    "INSERT INTO run_messages(run_id, session_id, ordinal, timeline_ordinal,
                    role, content_hash, name, tool_call_id, tool_calls)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
                 ON CONFLICT(run_id, ordinal) DO UPDATE SET
                    session_id=excluded.session_id,
                    timeline_ordinal=excluded.timeline_ordinal, role=excluded.role,
                    content_hash=excluded.content_hash, name=excluded.name,
                    tool_call_id=excluded.tool_call_id, tool_calls=excluded.tool_calls
                 WHERE run_messages.session_id IS NOT excluded.session_id
                    OR run_messages.timeline_ordinal IS NOT excluded.timeline_ordinal
                    OR run_messages.role IS NOT excluded.role
                    OR run_messages.content_hash IS NOT excluded.content_hash
                    OR run_messages.name IS NOT excluded.name
                    OR run_messages.tool_call_id IS NOT excluded.tool_call_id
                    OR run_messages.tool_calls IS NOT excluded.tool_calls",
                    params![
                        run_id_bytes.as_slice(),
                        session_id,
                        ordinal,
                        timeline_ordinal,
                        message_role_name(message.role),
                        content_hash,
                        message.name,
                        tool_call_id,
                        tool_calls
                    ],
                )
                .map_err(|error| {
                    persistence_error(format!("could not save run message: {error}"), true)
                })?;
            if fragments_match == Some(true) {
                delete_run_message_fragments(transaction, *run_id, ordinal)?;
            }
        }
        if prune_missing {
            transaction
                .execute(
                    "DELETE FROM run_messages WHERE run_id=?1 AND fragments='[]' AND NOT EXISTS (
                SELECT 1 FROM _loom_wanted_run_messages wanted
                WHERE wanted.run_id=run_messages.run_id AND wanted.ordinal=run_messages.ordinal
                )",
                    [run_id.as_uuid().as_bytes().as_slice()],
                )
                .map_err(|error| {
                    persistence_error(
                        format!("could not prune run messages for {run_id}: {error}"),
                        true,
                    )
                })?;
        }
    }
    collect_unused_content(transaction, MAX_CONTENT_GC_CANDIDATES_PER_WRITE)
}

fn save_session_rows(transaction: &Transaction<'_>, state: &SessionManagerState) -> Result<()> {
    let next_sequence = i64::try_from(state.next_sequence.value()).map_err(|_| {
        LoomError::new(
            ErrorCode::Persistence,
            "session lifecycle sequence exceeds SQLite's integer range",
            false,
        )
    })?;
    transaction
        .execute(
            "INSERT INTO session_store_meta(singleton, next_sequence) VALUES (1, ?1)
             ON CONFLICT(singleton) DO UPDATE SET next_sequence=excluded.next_sequence
             WHERE session_store_meta.next_sequence IS NOT excluded.next_sequence",
            [next_sequence],
        )
        .map_err(|error| {
            persistence_error(format!("could not save session sequence: {error}"), true)
        })?;
    transaction
        .execute_batch(
            "CREATE TEMP TABLE IF NOT EXISTS _loom_wanted_sessions (
                id BLOB PRIMARY KEY NOT NULL
             ) WITHOUT ROWID, STRICT;
             DELETE FROM _loom_wanted_sessions;",
        )
        .map_err(|error| {
            persistence_error(format!("could not stage session catalog: {error}"), true)
        })?;
    for (id, session) in &state.sessions {
        if *id != session.id {
            return Err(LoomError::new(
                ErrorCode::MalformedPayload,
                "session key does not match its snapshot id",
                false,
            ));
        }
        let id = session.id.as_uuid().as_bytes();
        let workspace_id = session.workspace_id.as_uuid().as_bytes();
        transaction
            .execute(
                "INSERT INTO _loom_wanted_sessions(id) VALUES (?1)",
                [id.as_slice()],
            )
            .map_err(|error| {
                persistence_error(format!("could not stage session catalog: {error}"), true)
            })?;
        transaction
            .execute(
                "INSERT INTO sessions(id, workspace_id, name, state, created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                 ON CONFLICT(id) DO UPDATE SET
                    workspace_id=excluded.workspace_id,
                    name=excluded.name,
                    state=excluded.state,
                    created_at=excluded.created_at,
                    updated_at=excluded.updated_at
                 WHERE sessions.workspace_id IS NOT excluded.workspace_id
                    OR sessions.name IS NOT excluded.name
                    OR sessions.state IS NOT excluded.state
                    OR sessions.created_at IS NOT excluded.created_at
                    OR sessions.updated_at IS NOT excluded.updated_at",
                params![
                    id.as_slice(),
                    workspace_id.as_slice(),
                    session.name,
                    session_state_name(session.state),
                    encode_timestamp(session.created_at)?,
                    encode_timestamp(session.updated_at)?,
                ],
            )
            .map_err(|error| {
                persistence_error(
                    format!("could not save session {}: {error}", session.id),
                    true,
                )
            })?;
        transaction
            .execute(
                "INSERT OR IGNORE INTO sessions_hierarchy
                    (project_id, session_id, parent_session_id, depth)
                 VALUES (?1, ?1, NULL, 1)",
                [id.as_slice()],
            )
            .map_err(|error| {
                persistence_error(
                    format!(
                        "could not initialize project root for session {}: {error}",
                        session.id
                    ),
                    true,
                )
            })?;
    }
    transaction
        .execute(
            "DELETE FROM sessions
             WHERE NOT EXISTS (
                SELECT 1 FROM _loom_wanted_sessions wanted WHERE wanted.id=sessions.id
             )",
            [],
        )
        .map_err(|error| {
            persistence_error(format!("could not prune session catalog: {error}"), true)
        })?;
    Ok(())
}

fn save_session_checkpoint_row(
    transaction: &Transaction<'_>,
    session: &AgentSessionSnapshot,
    next_sequence: EventSequence,
) -> Result<()> {
    let next_sequence = i64::try_from(next_sequence.value()).map_err(|_| {
        LoomError::new(
            ErrorCode::Persistence,
            "session lifecycle sequence exceeds SQLite's integer range",
            false,
        )
    })?;
    transaction
        .execute(
            "INSERT INTO session_store_meta(singleton, next_sequence) VALUES (1, ?1)
             ON CONFLICT(singleton) DO UPDATE SET
                next_sequence=MAX(session_store_meta.next_sequence, excluded.next_sequence)",
            [next_sequence],
        )
        .map_err(|error| {
            persistence_error(format!("could not save session sequence: {error}"), true)
        })?;
    transaction
        .execute(
            "INSERT INTO sessions(id, workspace_id, name, state, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(id) DO UPDATE SET
                workspace_id=excluded.workspace_id,
                name=excluded.name,
                state=excluded.state,
                created_at=excluded.created_at,
                updated_at=excluded.updated_at
             WHERE sessions.workspace_id IS NOT excluded.workspace_id
                OR sessions.name IS NOT excluded.name
                OR sessions.state IS NOT excluded.state
                OR sessions.created_at IS NOT excluded.created_at
                OR sessions.updated_at IS NOT excluded.updated_at",
            params![
                session.id.as_uuid().as_bytes().as_slice(),
                session.workspace_id.as_uuid().as_bytes().as_slice(),
                session.name,
                session_state_name(session.state),
                encode_timestamp(session.created_at)?,
                encode_timestamp(session.updated_at)?,
            ],
        )
        .map_err(|error| {
            persistence_error(
                format!("could not save session checkpoint {}: {error}", session.id),
                true,
            )
        })?;
    Ok(())
}

fn save_workspace_rows(transaction: &Transaction<'_>, state: &WorkspaceManagerState) -> Result<()> {
    transaction
        .execute_batch(
            "CREATE TEMP TABLE IF NOT EXISTS _loom_wanted_workspaces (
                id BLOB PRIMARY KEY NOT NULL
             ) WITHOUT ROWID, STRICT;
             DELETE FROM _loom_wanted_workspaces;",
        )
        .map_err(|error| {
            persistence_error(format!("could not stage workspace catalog: {error}"), true)
        })?;
    for (id, workspace) in &state.workspaces {
        if *id != workspace.id
            || workspace.name.trim().is_empty()
            || workspace.updated_at < workspace.created_at
        {
            return Err(LoomError::new(
                ErrorCode::MalformedPayload,
                "workspace catalog contains an inconsistent record",
                false,
            ));
        }
        let id_bytes = id.as_uuid().as_bytes();
        transaction
            .execute(
                "INSERT INTO _loom_wanted_workspaces(id) VALUES (?1)",
                [id_bytes.as_slice()],
            )
            .map_err(|error| {
                persistence_error(format!("could not stage workspace catalog: {error}"), true)
            })?;
        transaction
            .execute(
                "INSERT INTO workspaces(id, name, created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(id) DO UPDATE SET
                    name=excluded.name,
                    created_at=excluded.created_at,
                    updated_at=excluded.updated_at
                 WHERE workspaces.name IS NOT excluded.name
                    OR workspaces.created_at IS NOT excluded.created_at
                    OR workspaces.updated_at IS NOT excluded.updated_at",
                params![
                    id_bytes.as_slice(),
                    workspace.name,
                    encode_timestamp(workspace.created_at)?,
                    encode_timestamp(workspace.updated_at)?,
                ],
            )
            .map_err(|error| {
                persistence_error(format!("could not save workspace {id}: {error}"), true)
            })?;
    }
    transaction
        .execute(
            "DELETE FROM workspaces
             WHERE NOT EXISTS (
                SELECT 1 FROM _loom_wanted_workspaces wanted WHERE wanted.id=workspaces.id
             )",
            [],
        )
        .map_err(|error| {
            persistence_error(format!("could not prune workspace catalog: {error}"), true)
        })?;
    Ok(())
}

fn save_session_settings_rows(
    transaction: &Transaction<'_>,
    settings: &DurableSessionSettings,
) -> Result<()> {
    transaction
        .execute_batch(
            "CREATE TEMP TABLE IF NOT EXISTS _loom_wanted_session_settings (
                session_id BLOB PRIMARY KEY NOT NULL
             ) WITHOUT ROWID, STRICT;
             DELETE FROM _loom_wanted_session_settings;",
        )
        .map_err(|error| {
            persistence_error(format!("could not stage session settings: {error}"), true)
        })?;
    let ids = settings
        .approval_policies
        .keys()
        .chain(settings.auto_approve_actions.keys())
        .copied()
        .collect::<BTreeSet<_>>();
    for id in ids {
        let policy = settings
            .approval_policies
            .get(&id)
            .cloned()
            .unwrap_or_default();
        let auto_approve = settings
            .auto_approve_actions
            .get(&id)
            .copied()
            .unwrap_or_default();
        let session_id = id.as_uuid().as_bytes();
        transaction
            .execute(
                "INSERT INTO _loom_wanted_session_settings(session_id) VALUES (?1)",
                [session_id.as_slice()],
            )
            .map_err(|error| {
                persistence_error(format!("could not stage session settings: {error}"), true)
            })?;
        transaction
            .execute(
                "INSERT INTO session_settings(
                    session_id, policy_read, policy_write, policy_command,
                    policy_network, policy_destructive, auto_approve_actions
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
                 ON CONFLICT(session_id) DO UPDATE SET
                    policy_read=excluded.policy_read,
                    policy_write=excluded.policy_write,
                    policy_command=excluded.policy_command,
                    policy_network=excluded.policy_network,
                    policy_destructive=excluded.policy_destructive,
                    auto_approve_actions=excluded.auto_approve_actions
                 WHERE session_settings.policy_read IS NOT excluded.policy_read
                    OR session_settings.policy_write IS NOT excluded.policy_write
                    OR session_settings.policy_command IS NOT excluded.policy_command
                    OR session_settings.policy_network IS NOT excluded.policy_network
                    OR session_settings.policy_destructive IS NOT excluded.policy_destructive
                    OR session_settings.auto_approve_actions IS NOT excluded.auto_approve_actions",
                params![
                    session_id.as_slice(),
                    encode_policy_decision(policy.read),
                    encode_policy_decision(policy.write),
                    encode_policy_decision(policy.command),
                    encode_policy_decision(policy.network),
                    encode_policy_decision(policy.destructive),
                    auto_approve,
                ],
            )
            .map_err(|error| {
                persistence_error(
                    format!("could not save settings for session {id}: {error}"),
                    true,
                )
            })?;
    }
    transaction
        .execute(
            "DELETE FROM session_settings
             WHERE NOT EXISTS (
                SELECT 1 FROM _loom_wanted_session_settings wanted
                WHERE wanted.session_id=session_settings.session_id
             )",
            [],
        )
        .map_err(|error| {
            persistence_error(format!("could not prune session settings: {error}"), true)
        })?;
    Ok(())
}

fn save_workspace_config_rows(
    transaction: &Transaction<'_>,
    configs: &BTreeMap<WorkspaceId, WorkspaceConfig>,
) -> Result<()> {
    transaction
        .execute_batch(
            "CREATE TEMP TABLE IF NOT EXISTS _loom_wanted_workspace_configs (
                workspace_id BLOB PRIMARY KEY NOT NULL
             ) WITHOUT ROWID, STRICT;
             DELETE FROM _loom_wanted_workspace_configs;",
        )
        .map_err(|error| {
            persistence_error(format!("could not stage workspace configs: {error}"), true)
        })?;
    for (id, config) in configs {
        let revision = i64::try_from(config.revision).map_err(|_| {
            LoomError::new(
                ErrorCode::Persistence,
                "workspace configuration revision exceeds SQLite's integer range",
                false,
            )
        })?;
        let payload = serde_json::to_string(config).map_err(|error| {
            persistence_error(
                format!("could not encode workspace configuration: {error}"),
                false,
            )
        })?;
        if payload.len() > 16_384 {
            return Err(LoomError::new(
                ErrorCode::Persistence,
                "workspace configuration exceeds the maximum supported size",
                false,
            ));
        }
        let id_bytes = id.as_uuid().as_bytes();
        transaction
            .execute(
                "INSERT INTO _loom_wanted_workspace_configs(workspace_id) VALUES (?1)",
                [id_bytes.as_slice()],
            )
            .map_err(|error| {
                persistence_error(format!("could not stage workspace configs: {error}"), true)
            })?;
        transaction
            .execute(
                "INSERT INTO workspace_configs(workspace_id, revision, config)
                 VALUES (?1, ?2, ?3)
                 ON CONFLICT(workspace_id) DO UPDATE SET
                    revision=excluded.revision,
                    config=excluded.config
                 WHERE workspace_configs.revision IS NOT excluded.revision
                    OR workspace_configs.config IS NOT excluded.config",
                params![id_bytes.as_slice(), revision, payload],
            )
            .map_err(|error| {
                persistence_error(
                    format!("could not save config for workspace {id}: {error}"),
                    true,
                )
            })?;
    }
    transaction
        .execute(
            "DELETE FROM workspace_configs
             WHERE NOT EXISTS (
                SELECT 1 FROM _loom_wanted_workspace_configs wanted
                WHERE wanted.workspace_id=workspace_configs.workspace_id
             )",
            [],
        )
        .map_err(|error| {
            persistence_error(format!("could not prune workspace configs: {error}"), true)
        })?;
    Ok(())
}

fn save_provider_config_rows(
    transaction: &Transaction<'_>,
    configs: &BTreeMap<ProviderId, ProviderConfig>,
) -> Result<()> {
    for (id, config) in configs {
        if id != &config.id {
            return Err(LoomError::new(
                ErrorCode::MalformedPayload,
                "provider config key does not match its id",
                false,
            ));
        }
    }
    save_provider_json_rows(
        transaction,
        "provider_configs",
        "config",
        configs.iter(),
        65_536,
    )
}

fn save_provider_health_rows(
    transaction: &Transaction<'_>,
    health: &BTreeMap<ProviderId, ProviderHealth>,
) -> Result<()> {
    save_provider_json_rows(
        transaction,
        "provider_health",
        "health",
        health.iter(),
        16_384,
    )
}

fn save_usage_totals(transaction: &Transaction<'_>, ledger: &UsageLedger) -> Result<()> {
    transaction
        .execute_batch(
            "CREATE TEMP TABLE IF NOT EXISTS _loom_wanted_usage_totals (
                provider_id TEXT NOT NULL,
                model_id TEXT NOT NULL,
                PRIMARY KEY(provider_id, model_id)
             ) WITHOUT ROWID, STRICT;
             DELETE FROM _loom_wanted_usage_totals;",
        )
        .map_err(|error| {
            persistence_error(
                format!("could not stage provider usage totals: {error}"),
                true,
            )
        })?;
    for (key, summary) in &ledger.aggregates {
        transaction
            .execute(
                "INSERT INTO _loom_wanted_usage_totals(provider_id, model_id) VALUES (?1, ?2)",
                params![key.provider.as_str(), key.model.as_str()],
            )
            .map_err(|error| {
                persistence_error(
                    format!("could not stage provider usage totals: {error}"),
                    true,
                )
            })?;
        transaction
            .execute(
                "INSERT INTO provider_usage_totals(
                    provider_id, model_id, requests, input_tokens, output_tokens,
                    cached_input_tokens, cost_micros
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
                 ON CONFLICT(provider_id, model_id) DO UPDATE SET
                    requests=excluded.requests,
                    input_tokens=excluded.input_tokens,
                    output_tokens=excluded.output_tokens,
                    cached_input_tokens=excluded.cached_input_tokens,
                    cost_micros=excluded.cost_micros
                 WHERE provider_usage_totals.requests IS NOT excluded.requests
                    OR provider_usage_totals.input_tokens IS NOT excluded.input_tokens
                    OR provider_usage_totals.output_tokens IS NOT excluded.output_tokens
                    OR provider_usage_totals.cached_input_tokens IS NOT excluded.cached_input_tokens
                    OR provider_usage_totals.cost_micros IS NOT excluded.cost_micros",
                params![
                    key.provider.as_str(),
                    key.model.as_str(),
                    encode_counter(summary.requests, "provider request count")?,
                    encode_counter(summary.input_tokens, "provider input token count")?,
                    encode_counter(summary.output_tokens, "provider output token count")?,
                    encode_counter(summary.cached_input_tokens, "cached input token count")?,
                    encode_counter(summary.cost_micros, "provider usage cost")?,
                ],
            )
            .map_err(|error| {
                persistence_error(
                    format!("could not save provider usage totals: {error}"),
                    true,
                )
            })?;
    }
    transaction
        .execute(
            "DELETE FROM provider_usage_totals
             WHERE NOT EXISTS (
                SELECT 1 FROM _loom_wanted_usage_totals wanted
                WHERE wanted.provider_id=provider_usage_totals.provider_id
                  AND wanted.model_id=provider_usage_totals.model_id
             )",
            [],
        )
        .map_err(|error| {
            persistence_error(
                format!("could not prune provider usage totals: {error}"),
                true,
            )
        })?;
    Ok(())
}

fn save_idempotency_rows(
    transaction: &Transaction<'_>,
    records: &BTreeMap<RequestId, DurableIdempotencyRecord>,
) -> Result<()> {
    transaction
        .execute_batch(
            "CREATE TEMP TABLE IF NOT EXISTS _loom_wanted_idempotency (
                request_id BLOB PRIMARY KEY NOT NULL
             ) WITHOUT ROWID, STRICT;
             DELETE FROM _loom_wanted_idempotency;",
        )
        .map_err(|error| {
            persistence_error(
                format!("could not stage idempotency records: {error}"),
                true,
            )
        })?;
    for (id, record) in records {
        let request = serde_json::to_string(&record.request).map_err(|error| {
            persistence_error(
                format!("could not encode idempotency request: {error}"),
                false,
            )
        })?;
        let response = serde_json::to_string(&record.response).map_err(|error| {
            persistence_error(
                format!("could not encode idempotency response: {error}"),
                false,
            )
        })?;
        if request.len() > MAX_IDEMPOTENCY_PAYLOAD_BYTES
            || response.len() > MAX_IDEMPOTENCY_PAYLOAD_BYTES
        {
            return Err(LoomError::new(
                ErrorCode::Persistence,
                "idempotency request or response exceeds the maximum supported size",
                false,
            ));
        }
        let id_bytes = id.as_uuid().as_bytes();
        let request_hash = Sha256::digest(request.as_bytes());
        transaction
            .execute(
                "INSERT INTO _loom_wanted_idempotency(request_id) VALUES (?1)",
                [id_bytes.as_slice()],
            )
            .map_err(|error| {
                persistence_error(
                    format!("could not stage idempotency records: {error}"),
                    true,
                )
            })?;
        transaction
            .execute(
                "INSERT INTO idempotency_records(
                    request_id, created_at, expires_at, request_hash, request, response
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                 ON CONFLICT(request_id) DO UPDATE SET
                    created_at=excluded.created_at,
                    expires_at=excluded.expires_at,
                    request_hash=excluded.request_hash,
                    request=excluded.request,
                    response=excluded.response
                 WHERE idempotency_records.created_at IS NOT excluded.created_at
                    OR idempotency_records.expires_at IS NOT excluded.expires_at
                    OR idempotency_records.request_hash IS NOT excluded.request_hash
                    OR idempotency_records.request IS NOT excluded.request
                    OR idempotency_records.response IS NOT excluded.response",
                params![
                    id_bytes.as_slice(),
                    encode_timestamp(record.created_at)?,
                    record.expires_at.map(encode_timestamp).transpose()?,
                    request_hash.as_slice(),
                    request,
                    response,
                ],
            )
            .map_err(|error| {
                persistence_error(
                    format!("could not save idempotency record {id}: {error}"),
                    true,
                )
            })?;
    }
    transaction
        .execute(
            "DELETE FROM idempotency_records
             WHERE NOT EXISTS (
                SELECT 1 FROM _loom_wanted_idempotency wanted
                WHERE wanted.request_id=idempotency_records.request_id
             )",
            [],
        )
        .map_err(|error| {
            persistence_error(
                format!("could not prune idempotency records: {error}"),
                true,
            )
        })?;
    Ok(())
}

fn save_provider_json_rows<'a, T, I>(
    transaction: &Transaction<'_>,
    table: &str,
    value_column: &str,
    rows: I,
    maximum_bytes: usize,
) -> Result<()>
where
    T: Serialize + 'a,
    I: IntoIterator<Item = (&'a ProviderId, &'a T)>,
{
    transaction
        .execute_batch(
            "CREATE TEMP TABLE IF NOT EXISTS _loom_wanted_providers (
                provider_id TEXT PRIMARY KEY NOT NULL
             ) WITHOUT ROWID, STRICT;
             DELETE FROM _loom_wanted_providers;",
        )
        .map_err(|error| persistence_error(format!("could not stage {table}: {error}"), true))?;
    for (id, value) in rows {
        if id.as_str().trim().is_empty() {
            return Err(LoomError::new(
                ErrorCode::MalformedPayload,
                "provider record has an empty id",
                false,
            ));
        }
        let payload = serde_json::to_string(value).map_err(|error| {
            persistence_error(format!("could not encode {table} record: {error}"), false)
        })?;
        if payload.len() > maximum_bytes {
            return Err(LoomError::new(
                ErrorCode::Persistence,
                format!("{table} record exceeds the maximum supported size"),
                false,
            ));
        }
        transaction
            .execute(
                "INSERT INTO _loom_wanted_providers(provider_id) VALUES (?1)",
                [id.as_str()],
            )
            .map_err(|error| {
                persistence_error(format!("could not stage {table}: {error}"), true)
            })?;
        let upsert = format!(
            "INSERT INTO {table}(provider_id, {value_column}) VALUES (?1, ?2)
             ON CONFLICT(provider_id) DO UPDATE SET {value_column}=excluded.{value_column}
             WHERE {table}.{value_column} IS NOT excluded.{value_column}"
        );
        transaction
            .execute(&upsert, params![id.as_str(), payload])
            .map_err(|error| persistence_error(format!("could not save {table}: {error}"), true))?;
    }
    let prune = format!(
        "DELETE FROM {table} WHERE NOT EXISTS (
            SELECT 1 FROM _loom_wanted_providers wanted
            WHERE wanted.provider_id={table}.provider_id
        )"
    );
    transaction
        .execute(&prune, [])
        .map_err(|error| persistence_error(format!("could not prune {table}: {error}"), true))?;
    Ok(())
}

fn session_state_name(state: AgentSessionState) -> &'static str {
    match state {
        AgentSessionState::Idle => "idle",
        AgentSessionState::Queued => "queued",
        AgentSessionState::Planning => "planning",
        AgentSessionState::AwaitingApproval => "awaiting_approval",
        AgentSessionState::Paused => "paused",
        AgentSessionState::Executing => "executing",
        AgentSessionState::Evaluating => "evaluating",
        AgentSessionState::NeedsInput => "needs_input",
        AgentSessionState::Completed => "completed",
        AgentSessionState::Failed => "failed",
        AgentSessionState::Cancelled => "cancelled",
        AgentSessionState::Archived => "archived",
    }
}

fn run_state_name(state: AgentRunState) -> &'static str {
    match state {
        AgentRunState::Planning => "planning",
        AgentRunState::Executing => "executing",
        AgentRunState::AwaitingApproval => "awaiting_approval",
        AgentRunState::Paused => "paused",
        AgentRunState::NeedsInput => "needs_input",
        AgentRunState::Evaluating => "evaluating",
        AgentRunState::Completed => "completed",
        AgentRunState::Failed => "failed",
        AgentRunState::Cancelled => "cancelled",
    }
}

fn parse_run_state(state: &str) -> Result<AgentRunState> {
    match state {
        "planning" => Ok(AgentRunState::Planning),
        "executing" => Ok(AgentRunState::Executing),
        "awaiting_approval" => Ok(AgentRunState::AwaitingApproval),
        "paused" => Ok(AgentRunState::Paused),
        "needs_input" => Ok(AgentRunState::NeedsInput),
        "evaluating" => Ok(AgentRunState::Evaluating),
        "completed" => Ok(AgentRunState::Completed),
        "failed" => Ok(AgentRunState::Failed),
        "cancelled" => Ok(AgentRunState::Cancelled),
        _ => Err(LoomError::new(
            ErrorCode::MalformedPayload,
            "persisted run attempt state is unknown",
            false,
        )),
    }
}

fn workspace_control_name(control: WorkspaceControl) -> &'static str {
    match control {
        WorkspaceControl::Agent => "agent",
        WorkspaceControl::User => "user",
    }
}

fn parse_workspace_control(control: &str) -> Result<WorkspaceControl> {
    match control {
        "agent" => Ok(WorkspaceControl::Agent),
        "user" => Ok(WorkspaceControl::User),
        _ => Err(LoomError::new(
            ErrorCode::MalformedPayload,
            "persisted filesystem has an unknown control state",
            false,
        )),
    }
}

fn workspace_change_kind_name(kind: WorkspaceChangeKind) -> &'static str {
    match kind {
        WorkspaceChangeKind::Created => "created",
        WorkspaceChangeKind::Modified => "modified",
        WorkspaceChangeKind::Deleted => "deleted",
    }
}

fn parse_workspace_change_kind(kind: &str) -> Result<WorkspaceChangeKind> {
    match kind {
        "created" => Ok(WorkspaceChangeKind::Created),
        "modified" => Ok(WorkspaceChangeKind::Modified),
        "deleted" => Ok(WorkspaceChangeKind::Deleted),
        _ => Err(LoomError::new(
            ErrorCode::MalformedPayload,
            "persisted filesystem change has an unknown kind",
            false,
        )),
    }
}

fn parse_session_state(state: &str) -> Result<AgentSessionState> {
    match state {
        "idle" => Ok(AgentSessionState::Idle),
        "queued" => Ok(AgentSessionState::Queued),
        "planning" => Ok(AgentSessionState::Planning),
        "awaiting_approval" => Ok(AgentSessionState::AwaitingApproval),
        "paused" => Ok(AgentSessionState::Paused),
        "executing" => Ok(AgentSessionState::Executing),
        "evaluating" => Ok(AgentSessionState::Evaluating),
        "needs_input" => Ok(AgentSessionState::NeedsInput),
        "completed" => Ok(AgentSessionState::Completed),
        "failed" => Ok(AgentSessionState::Failed),
        "cancelled" => Ok(AgentSessionState::Cancelled),
        "archived" => Ok(AgentSessionState::Archived),
        _ => Err(LoomError::new(
            ErrorCode::MalformedPayload,
            format!("persisted session has an unknown state '{state}'"),
            false,
        )),
    }
}

fn encode_timestamp(timestamp: Timestamp) -> Result<i64> {
    i64::try_from(timestamp.as_unix_millis()).map_err(|_| {
        LoomError::new(
            ErrorCode::Persistence,
            "timestamp exceeds SQLite's integer range",
            false,
        )
    })
}

fn encode_counter(counter: u64, field: &str) -> Result<i64> {
    i64::try_from(counter).map_err(|_| {
        LoomError::new(
            ErrorCode::Persistence,
            format!("{field} exceeds SQLite's integer range"),
            false,
        )
    })
}

fn decode_counter(counter: i64, field: &str) -> Result<u64> {
    u64::try_from(counter).map_err(|_| {
        LoomError::new(
            ErrorCode::MalformedPayload,
            format!("persisted {field} is negative"),
            false,
        )
    })
}

fn decode_optional_u64(counter: Option<i64>, field: &str) -> Result<Option<u64>> {
    counter
        .map(|value| decode_counter(value, field))
        .transpose()
}

fn encode_optional_counter(counter: Option<u64>, field: &str) -> Result<Option<i64>> {
    counter
        .map(|value| encode_counter(value, field))
        .transpose()
}

fn encode_policy_decision(decision: PolicyDecision) -> &'static str {
    match decision {
        PolicyDecision::Allow => "allow",
        PolicyDecision::RequireApproval => "require_approval",
        PolicyDecision::Deny => "deny",
    }
}

fn decode_policy_decision(value: &str, action: &str) -> Result<PolicyDecision> {
    match value {
        "allow" => Ok(PolicyDecision::Allow),
        "require_approval" => Ok(PolicyDecision::RequireApproval),
        "deny" => Ok(PolicyDecision::Deny),
        _ => Err(LoomError::new(
            ErrorCode::MalformedPayload,
            format!("persisted {action} approval decision is invalid"),
            false,
        )),
    }
}

fn decode_timestamp(timestamp: i64) -> Result<Timestamp> {
    u64::try_from(timestamp)
        .map(Timestamp::from_unix_millis)
        .map_err(|_| {
            LoomError::new(
                ErrorCode::MalformedPayload,
                "persisted timestamp is negative",
                false,
            )
        })
}

fn decode_uuid(bytes: &[u8], field: &str) -> Result<Uuid> {
    Uuid::from_slice(bytes).map_err(|error| {
        LoomError::new(
            ErrorCode::MalformedPayload,
            format!("persisted {field} is invalid: {error}"),
            false,
        )
    })
}

fn persistence_error(message: String, retryable: bool) -> LoomError {
    LoomError::new(ErrorCode::Persistence, message, retryable)
}

fn decode_json<T: DeserializeOwned>(payload: &str, field: &str) -> Result<T> {
    serde_json::from_str(payload).map_err(|error| {
        LoomError::new(
            ErrorCode::MalformedPayload,
            format!("persisted {field} is malformed: {error}"),
            false,
        )
    })
}

pub type DurableStore = FilePersistence;

#[derive(Clone, Debug, Default)]
pub struct MemoryPersistence {
    value: Arc<Mutex<Option<Value>>>,
}

impl MemoryPersistence {
    pub fn load<T: DeserializeOwned>(&self) -> Result<Option<T>> {
        let value = self
            .value
            .lock()
            .map_err(|_| {
                LoomError::new(
                    ErrorCode::Internal,
                    "memory persistence lock was poisoned",
                    true,
                )
            })?
            .clone();
        value
            .map(|value| {
                serde_json::from_value(value).map_err(|error| {
                    LoomError::new(
                        ErrorCode::MalformedPayload,
                        format!("memory persistence contains malformed data: {error}"),
                        false,
                    )
                })
            })
            .transpose()
    }

    pub fn save<T: Serialize>(&self, value: &T) -> Result<()> {
        let value = serde_json::to_value(value).map_err(|error| {
            LoomError::new(
                ErrorCode::Persistence,
                format!("could not serialize memory state: {error}"),
                false,
            )
        })?;
        *self.value.lock().map_err(|_| {
            LoomError::new(
                ErrorCode::Internal,
                "memory persistence lock was poisoned",
                true,
            )
        })? = Some(value);
        Ok(())
    }

    pub fn set_raw(&self, value: Value) -> Result<()> {
        *self.value.lock().map_err(|_| {
            LoomError::new(
                ErrorCode::Internal,
                "memory persistence lock was poisoned",
                true,
            )
        })? = Some(value);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use loom_session::{SessionManager, WorkspaceManager};
    use serde::{Deserialize, Serialize};
    use std::process::Command;
    use uuid::Uuid;

    // The cross-process owner-lock test spawns a child while holding a lock
    // file. Serialize it with the drop/reacquire tests so a forked child cannot
    // transiently retain another test's flock descriptor.
    static EXCLUSIVE_WRITER_TEST_LOCK: Mutex<()> = Mutex::new(());

    #[derive(Debug, Deserialize, PartialEq, Serialize)]
    struct Fixture {
        value: String,
    }

    fn stored_fragment_count(path: &std::path::Path, run_id: RunId) -> usize {
        let connection = Connection::open(path).unwrap();
        let mut statement = connection
            .prepare("SELECT fragments FROM run_messages WHERE run_id=?1")
            .unwrap();
        let counts = statement
            .query_map([run_id.as_uuid().as_bytes().as_slice()], |row| {
                row.get::<_, String>(0)
            })
            .unwrap()
            .map(|row| decode_stored_fragments(&row.unwrap()).unwrap().len())
            .collect::<Vec<_>>();
        counts.into_iter().sum()
    }

    fn invalid_feed_for_rollback() -> DurableFeedState {
        DurableFeedState {
            next_sequence: EventSequence::new(u64::MAX),
            retention_limit: 250,
            events: Vec::new(),
            workspace_events: Vec::new(),
        }
    }

    #[test]
    fn session_projection_reader_keeps_run_and_cursor_on_one_sqlite_snapshot() {
        let path = std::env::temp_dir().join(format!("loom-persistence-{}.db", Uuid::new_v4()));
        let store = FilePersistence::open(&path).unwrap();
        store
            .save_state_with_sessions(&SessionManager::default().export_state())
            .unwrap();

        let session_id = AgentSessionId::new();
        let workspace_id = WorkspaceId::new();
        let run_id = RunId::new();
        let attempt_id = RunAttemptId::new();
        {
            let setup = Connection::open(&path).unwrap();
            setup
                .execute(
                    "INSERT INTO sessions(id, workspace_id, name, state, created_at, updated_at)
                 VALUES (?1, ?2, 'snapshot', 'idle', 1, 1)",
                    params![
                        session_id.as_uuid().as_bytes().as_slice(),
                        workspace_id.as_uuid().as_bytes().as_slice()
                    ],
                )
                .unwrap();
            setup
                .execute(
                    "INSERT INTO run_summaries(
                    run_id, session_id, attempt_id, control_revision, state, started_at,
                    updated_at, completed_at, task, model, summary, input_tokens,
                    output_tokens, cached_input_tokens, tool_calls, cost_micros, elapsed_ms
                 ) VALUES (?1, ?2, ?3, 1, 'executing', 1, 1, NULL, 'before', 'model', NULL,
                           0, 0, 0, 0, 0, 0)",
                    params![
                        run_id.as_uuid().as_bytes().as_slice(),
                        session_id.as_uuid().as_bytes().as_slice(),
                        attempt_id.as_uuid().as_bytes().as_slice()
                    ],
                )
                .unwrap();
            setup.execute(
                "INSERT INTO feed_session_meta(session_id, first_sequence, latest_sequence, pruned_through)
                 VALUES (?1, 1, 1, 0)",
                [session_id.as_uuid().as_bytes().as_slice()],
            ).unwrap();
        }

        let read = store
            .load_session_projection_read_between(session_id, || {
                let writer = Connection::open(&path).unwrap();
                let transaction = writer.unchecked_transaction().unwrap();
                transaction
                    .execute(
                        "UPDATE run_summaries SET task='after', updated_at=2 WHERE run_id=?1",
                        [run_id.as_uuid().as_bytes().as_slice()],
                    )
                    .unwrap();
                transaction
                    .execute(
                        "UPDATE feed_session_meta SET latest_sequence=2 WHERE session_id=?1",
                        [session_id.as_uuid().as_bytes().as_slice()],
                    )
                    .unwrap();
                transaction.commit().unwrap();
                Ok(())
            })
            .unwrap();
        assert_eq!(read.latest_run.as_ref().unwrap().snapshot.task, "before");
        assert_eq!(read.latest_sequence, Some(EventSequence::new(1)));

        let fresh = store.load_session_projection_read(session_id).unwrap();
        assert_eq!(fresh.latest_run.as_ref().unwrap().snapshot.task, "after");
        assert_eq!(fresh.latest_sequence, Some(EventSequence::new(2)));
        drop(store);
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn run_checkpoint_is_scoped_and_rolls_back_session_and_run_with_feed_failure() {
        let path = std::env::temp_dir().join(format!("loom-persistence-{}.db", Uuid::new_v4()));
        let persistence = FilePersistence::open(&path).unwrap();
        let mut sessions = SessionManager::default();
        let (session, _) = sessions
            .create_in_workspace(WorkspaceId::new(), "worker session")
            .unwrap();
        let run_id = RunId::new();
        let other_run_id = RunId::new();
        let make_summary = |id, task: &str| DurableRunSummary {
            snapshot: AgentRunSnapshot {
                id,
                attempt_id: loom_core::RunAttemptId::new(),
                control_revision: 0,
                session_id: session.id,
                task: task.to_owned(),
                model: ModelId::new("deterministic-model"),
                state: AgentRunState::Planning,
                started_at: Timestamp::from_unix_millis(1),
                updated_at: Timestamp::from_unix_millis(1),
                completed_at: None,
                summary: None,
                evidence: Vec::new(),
            },
            usage: UsageSnapshot::default(),
            attempts: None,
            execution_state: None,
            interactions: None,
        };
        let initial_summary = make_summary(run_id, "before");
        let other_summary = make_summary(other_run_id, "unrelated run");
        let initial_runs = BTreeMap::from([
            (run_id, initial_summary.clone()),
            (other_run_id, other_summary.clone()),
        ]);
        let activity_call = loom_model::ToolCall {
            id: loom_core::ToolCallId::new(),
            name: "read_file".to_owned(),
            arguments: serde_json::json!({"path": "src/lib.rs"}),
        };
        let initial_activity = AgentActivityRecord {
            id: ActivityId::new(),
            run_id,
            timeline_ordinal: 1,
            parent_id: None,
            step_id: None,
            kind: AgentActivityKind::ToolCall,
            status: AgentActivityStatus::AwaitingApproval,
            started_at: Timestamp::from_unix_millis(2),
            completed_at: None,
            elapsed_ms: None,
            data: AgentActivityData::ToolCall {
                call: activity_call.clone(),
                result: None,
            },
        };
        let initial_activities = BTreeMap::from([(run_id, vec![initial_activity.clone()])]);
        let initial_transcript = BTreeMap::from([(
            run_id,
            vec![
                DurableRunMessage {
                    timeline_ordinal: 0,
                    role: loom_model::MessageRole::System,
                    content: "old system".to_owned(),
                    name: None,
                    tool_call_id: None,
                    tool_calls: Vec::new(),
                },
                DurableRunMessage {
                    timeline_ordinal: 2,
                    role: loom_model::MessageRole::Assistant,
                    content: "stale streamed answer".to_owned(),
                    name: Some("old header".to_owned()),
                    tool_call_id: None,
                    tool_calls: Vec::new(),
                },
            ],
        )]);
        persistence
            .save_state(DurableStateWrite {
                sessions: &sessions.export_state(),
                workspaces: None,
                settings: None,
                workspace_configs: None,
                providers: None,
                usage: None,
                idempotency: None,
                run_summaries: Some(&initial_runs),
                run_runtime_configs: None,
                run_context_checkpoints: None,
                run_plans: None,
                run_messages: Some(&initial_transcript),
                run_activities: Some(&initial_activities),
                filesystem_records: None,
                feed: None,
            })
            .unwrap();

        let mut updated_summary = initial_summary.clone();
        updated_summary.snapshot.task = "after checkpoint".to_owned();
        let runtime_config = DurableRunRuntimeConfig {
            system_instructions: None,
            repository_instructions: None,
            approval_policy: ApprovalPolicy::default(),
            limits: loom_core::SessionLimits::default(),
            context_options: ContextAssemblyOptions::default(),
            checkpoint_id: None,
            input_cost_micros_per_1k: 0,
            output_cost_micros_per_1k: 0,
            context_inspection: None,
            project_delegation_enabled: false,
            project_messaging_enabled: false,
            project_inspection_enabled: false,
            project_child_control_enabled: false,
            project_worktree_enabled: false,
            project_review_enabled: false,
            project_integration_enabled: false,
            project_branch_messaging_enabled: false,
        };
        let mut changed_session = session.clone();
        changed_session.state = AgentSessionState::Planning;
        changed_session.updated_at = Timestamp::now();
        let invalid_session_id = AgentSessionId::new();
        let invalid_feed = DurableFeedState {
            next_sequence: EventSequence::new(1),
            retention_limit: 250,
            events: vec![ServerEventEnvelope {
                protocol_version: loom_protocol::CURRENT_PROTOCOL_VERSION,
                sequence: EventSequence::new(1),
                session_id: invalid_session_id,
                event: loom_protocol::ServerEvent::AgentSessionCreated {
                    snapshot: changed_session.clone(),
                },
            }],
            workspace_events: Vec::new(),
        };
        let plan = loom_protocol::AgentPlan { steps: Vec::new() };
        let mut updated_activity = initial_activity.clone();
        updated_activity.status = AgentActivityStatus::Completed;
        updated_activity.completed_at = Some(Timestamp::from_unix_millis(3));
        updated_activity.elapsed_ms = Some(1);
        let activity_result = ToolResult {
            tool_call_id: activity_call.id,
            name: activity_call.name.clone(),
            success: true,
            output: "first attempt result".to_owned(),
        };
        updated_activity.data = AgentActivityData::ToolCall {
            call: activity_call.clone(),
            result: Some(activity_result.clone()),
        };
        let second_activity = AgentActivityRecord {
            id: ActivityId::new(),
            run_id,
            timeline_ordinal: 4,
            parent_id: None,
            step_id: None,
            kind: AgentActivityKind::ToolCall,
            status: AgentActivityStatus::Started,
            started_at: Timestamp::from_unix_millis(4),
            completed_at: None,
            elapsed_ms: None,
            data: AgentActivityData::ToolCall {
                call: activity_call.clone(),
                result: None,
            },
        };
        let third_activity = AgentActivityRecord {
            id: ActivityId::new(),
            run_id,
            timeline_ordinal: 5,
            parent_id: None,
            step_id: None,
            kind: AgentActivityKind::ModelTurn,
            status: AgentActivityStatus::Completed,
            started_at: Timestamp::from_unix_millis(5),
            completed_at: Some(Timestamp::from_unix_millis(6)),
            elapsed_ms: Some(1),
            data: AgentActivityData::ModelTurn {
                model: ModelId::new("deterministic-model"),
            },
        };
        let activity_delta = vec![
            updated_activity.clone(),
            second_activity.clone(),
            third_activity.clone(),
        ];
        let retry_transcript = DurableRunMessageDelta {
            start_ordinal: 0,
            reset: true,
            messages: vec![
                DurableRunMessage {
                    timeline_ordinal: 3,
                    role: loom_model::MessageRole::User,
                    content: "retry prompt".to_owned(),
                    name: None,
                    tool_call_id: None,
                    tool_calls: Vec::new(),
                },
                DurableRunMessage {
                    timeline_ordinal: 6,
                    role: loom_model::MessageRole::Assistant,
                    content: "partial".to_owned(),
                    name: Some("streaming".to_owned()),
                    tool_call_id: None,
                    tool_calls: Vec::new(),
                },
            ],
        };
        let invalid_checkpoint = DurableRunCheckpointWrite {
            session: &changed_session,
            session_next_sequence: EventSequence::new(1),
            prune_feed: false,
            summary: &updated_summary,
            runtime_config: &runtime_config,
            context_checkpoint: None,
            plan: &plan,
            messages: &[],
            message_delta: Some(&retry_transcript),
            activities: &[],
            activity_deltas: Some(&activity_delta),
            filesystem: None,
            feed: &invalid_feed,
        };
        assert!(persistence.save_run_checkpoint(invalid_checkpoint).is_err());
        assert_eq!(
            persistence.load_run_summary(run_id).unwrap().unwrap(),
            initial_summary,
            "feed failure must roll back the run summary"
        );
        assert_eq!(
            persistence
                .load_sessions()
                .unwrap()
                .unwrap()
                .sessions
                .get(&session.id)
                .unwrap()
                .state,
            AgentSessionState::Idle,
            "feed failure must roll back the owning session projection"
        );
        assert_eq!(
            persistence.load_run_summary(other_run_id).unwrap().unwrap(),
            other_summary,
            "a worker checkpoint must leave unrelated runs untouched"
        );
        assert_eq!(
            persistence.load_run_activities(run_id).unwrap(),
            vec![initial_activity.clone()],
            "feed failure must roll back activity updates and appends"
        );
        assert_eq!(
            persistence.load_run_tool_attempts(run_id).unwrap()[0].attempt_number,
            1,
            "feed failure must roll back tool attempt updates"
        );
        assert_eq!(
            persistence.load_run_messages(run_id).unwrap(),
            initial_transcript[&run_id],
            "a failed retry checkpoint must retain the previous transcript generation"
        );

        let valid_feed = DurableFeedState {
            next_sequence: EventSequence::new(1),
            retention_limit: 250,
            events: vec![ServerEventEnvelope {
                protocol_version: loom_protocol::CURRENT_PROTOCOL_VERSION,
                sequence: EventSequence::new(1),
                session_id: session.id,
                event: loom_protocol::ServerEvent::AgentSessionCreated {
                    snapshot: changed_session.clone(),
                },
            }],
            workspace_events: Vec::new(),
        };
        let valid_checkpoint = DurableRunCheckpointWrite {
            session: &changed_session,
            session_next_sequence: EventSequence::new(1),
            prune_feed: false,
            summary: &updated_summary,
            runtime_config: &runtime_config,
            context_checkpoint: None,
            plan: &plan,
            messages: &[],
            message_delta: Some(&retry_transcript),
            activities: &[],
            activity_deltas: Some(&activity_delta),
            filesystem: None,
            feed: &valid_feed,
        };
        persistence.save_run_checkpoint(valid_checkpoint).unwrap();
        persistence
            .append_run_message_fragment(run_id, session.id, 1, 0, 7, b" final")
            .unwrap();
        let tail_delta = DurableRunMessageDelta {
            start_ordinal: 1,
            reset: false,
            messages: vec![DurableRunMessage {
                timeline_ordinal: 6,
                role: loom_model::MessageRole::Assistant,
                content: "partial final".to_owned(),
                name: Some("new header".to_owned()),
                tool_call_id: None,
                tool_calls: Vec::new(),
            }],
        };
        let empty_feed = DurableFeedState {
            next_sequence: EventSequence::new(1),
            retention_limit: 250,
            events: Vec::new(),
            workspace_events: Vec::new(),
        };
        persistence
            .save_run_checkpoint(DurableRunCheckpointWrite {
                session: &changed_session,
                session_next_sequence: EventSequence::new(1),
                prune_feed: false,
                summary: &updated_summary,
                runtime_config: &runtime_config,
                context_checkpoint: None,
                plan: &plan,
                messages: &[],
                message_delta: Some(&tail_delta),
                activities: &[],
                activity_deltas: Some(&[]),
                filesystem: None,
                feed: &empty_feed,
            })
            .unwrap();
        drop(persistence);
        let reopened = FilePersistence::open(&path).unwrap();
        assert_eq!(
            reopened.load_run_activities(run_id).unwrap(),
            vec![updated_activity, second_activity, third_activity],
            "activity update and append order must survive restart"
        );
        assert_eq!(
            reopened.load_run_messages(run_id).unwrap(),
            vec![
                retry_transcript.messages[0].clone(),
                tail_delta.messages[0].clone()
            ],
            "retry reset and active assistant header update must survive restart"
        );
        assert_eq!(
            reopened
                .load_run_tool_attempts(run_id)
                .unwrap()
                .iter()
                .map(|attempt| attempt.attempt_number)
                .collect::<Vec<_>>(),
            vec![1, 2],
            "per-activity writes must retain logical tool attempt order"
        );
        let attempts = reopened.load_run_tool_attempts(run_id).unwrap();
        assert_eq!(attempts[0].result.as_ref(), Some(&activity_result));
        assert_eq!(
            reopened.load_run_tool_calls(run_id).unwrap().len(),
            1,
            "adding a non-tool activity must not prune logical tool-call rows"
        );
        assert_eq!(
            reopened
                .load_sessions()
                .unwrap()
                .unwrap()
                .sessions
                .get(&session.id)
                .unwrap()
                .state,
            AgentSessionState::Planning
        );
        assert_eq!(
            reopened
                .load_run_summary(run_id)
                .unwrap()
                .unwrap()
                .snapshot
                .task,
            "after checkpoint"
        );
        assert_eq!(
            reopened.load_run_summary(other_run_id).unwrap().unwrap(),
            other_summary
        );
        assert_eq!(
            reopened
                .load_feed_events_since(Some(session.id), None)
                .unwrap()
                .len(),
            1,
            "feed rows and the session/run projections commit together"
        );
        drop(reopened);
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn sqlite_connections_are_lazy_shared_by_clones_and_observe_other_handles() {
        let path = std::env::temp_dir().join(format!("loom-persistence-{}.db", Uuid::new_v4()));
        let first = FilePersistence::open(&path).unwrap();
        let clone = first.clone();
        assert!(!first.exists());
        assert!(first.load_sessions().unwrap().is_none());
        assert!(!first.exists(), "read-only construction must stay lazy");
        assert!(Arc::ptr_eq(&first.connection, &clone.connection));
        drop((first, clone));
    }

    #[test]
    fn exclusive_writer_ownership_is_shared_by_clones_and_released_on_drop() {
        let _guard = EXCLUSIVE_WRITER_TEST_LOCK.lock().unwrap();
        let test_dir = std::env::temp_dir().join(format!(
            "loom-persistence-owner-{}-{}",
            std::process::id(),
            Uuid::new_v4()
        ));
        fs::create_dir(&test_dir).unwrap();
        let path = test_dir.join("state.db");
        let writer = FilePersistence::open_exclusive_writer(&path).unwrap();
        let clone = writer.clone();
        assert_eq!(
            FilePersistence::open_exclusive_writer(&path)
                .unwrap_err()
                .code,
            ErrorCode::Conflict
        );
        // Diagnostic handles do not claim backend ownership.
        assert!(FilePersistence::open(&path).is_ok());
        drop(writer);
        assert_eq!(
            FilePersistence::open_exclusive_writer(&path)
                .unwrap_err()
                .code,
            ErrorCode::Conflict
        );
        // The shared lock remains owned until the final clone is dropped.
        drop(clone);
        let replacement = FilePersistence::open_exclusive_writer(&path).unwrap();
        drop(replacement);
        let mut lock_path = path.as_os_str().to_os_string();
        lock_path.push(".loom-owner.lock");
        fs::remove_file(PathBuf::from(lock_path)).unwrap();
        fs::remove_dir(test_dir).unwrap();
    }

    #[test]
    fn exclusive_writer_can_be_released_explicitly_through_a_clone() {
        let _guard = EXCLUSIVE_WRITER_TEST_LOCK.lock().unwrap();
        let test_dir = std::env::temp_dir().join(format!(
            "loom-persistence-owner-{}-{}",
            std::process::id(),
            Uuid::new_v4()
        ));
        fs::create_dir(&test_dir).unwrap();
        let path = test_dir.join("state.db");
        let writer = FilePersistence::open_exclusive_writer(&path).unwrap();
        let clone = writer.clone();
        drop(writer);
        assert_eq!(
            FilePersistence::open_exclusive_writer(&path)
                .unwrap_err()
                .code,
            ErrorCode::Conflict
        );
        clone.release_exclusive_writer().unwrap();
        let replacement = FilePersistence::open_exclusive_writer(&path).unwrap();
        drop(replacement);
        drop(clone);
        let mut lock_path = path.as_os_str().to_os_string();
        lock_path.push(".loom-owner.lock");
        fs::remove_file(PathBuf::from(lock_path)).unwrap();
        fs::remove_dir(test_dir).unwrap();
    }

    fn run_exclusive_writer_probe(path: &Path, should_be_owned: bool) {
        let status = Command::new(std::env::current_exe().unwrap())
            .arg("--exact")
            .arg("tests::exclusive_writer_subprocess_probe")
            .arg("--nocapture")
            .env("LOOM_TEST_OWNER_PROBE_PATH", path)
            .env("LOOM_TEST_OWNER_EXPECTED", should_be_owned.to_string())
            .status()
            .unwrap();
        assert!(status.success(), "subprocess ownership probe failed");
    }

    #[test]
    fn exclusive_writer_subprocess_probe() {
        let Ok(path) = std::env::var("LOOM_TEST_OWNER_PROBE_PATH") else {
            return;
        };
        let should_be_owned = std::env::var("LOOM_TEST_OWNER_EXPECTED")
            .unwrap()
            .parse::<bool>()
            .unwrap();
        match FilePersistence::open_exclusive_writer(path) {
            Ok(writer) => {
                assert!(!should_be_owned, "another process should own this database");
                drop(writer);
            }
            Err(error) => {
                assert!(
                    should_be_owned,
                    "unexpected exclusive writer error: {error}"
                );
                assert_eq!(error.code, ErrorCode::Conflict);
            }
        }
    }

    #[test]
    fn exclusive_writer_lock_is_enforced_across_processes() {
        let _guard = EXCLUSIVE_WRITER_TEST_LOCK.lock().unwrap();
        let test_dir = std::env::temp_dir().join(format!(
            "loom-persistence-owner-{}-{}",
            std::process::id(),
            Uuid::new_v4()
        ));
        fs::create_dir(&test_dir).unwrap();
        let path = test_dir.join("state.db");
        let writer = FilePersistence::open_exclusive_writer(&path).unwrap();
        run_exclusive_writer_probe(&path, true);
        drop(writer);
        run_exclusive_writer_probe(&path, false);
        let mut lock_path = path.as_os_str().to_os_string();
        lock_path.push(".loom-owner.lock");
        fs::remove_file(PathBuf::from(lock_path)).unwrap();
        fs::remove_dir(test_dir).unwrap();
    }

    #[test]
    fn non_sqlite_file_is_rejected_without_migration() {
        let path = std::env::temp_dir().join(format!("loom-persistence-{}.db", Uuid::new_v4()));
        fs::write(&path, b"{not json").unwrap();
        let store = FilePersistence::open(&path).unwrap();
        let error = store.load_feed_header().unwrap_err();
        assert_eq!(error.code, ErrorCode::Persistence);
        assert!(!path.with_extension("json.legacy").exists());
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn memory_store_round_trips_and_rejects_malformed_values() {
        let store = MemoryPersistence::default();
        assert!(store.load::<Fixture>().unwrap().is_none());
        store
            .save(&Fixture {
                value: "memory".to_owned(),
            })
            .unwrap();
        assert_eq!(
            store.load::<Fixture>().unwrap(),
            Some(Fixture {
                value: "memory".to_owned(),
            })
        );
        store.set_raw(serde_json::json!({"wrong": true})).unwrap();
        assert_eq!(
            store.load::<Fixture>().unwrap_err().code,
            ErrorCode::MalformedPayload
        );
    }

    #[test]
    fn persistence_rejects_empty_paths() {
        assert_eq!(
            FilePersistence::open("").unwrap_err().code,
            ErrorCode::InvalidRequest
        );
    }

    #[test]
    fn missing_database_reads_stay_lazy_and_return_empty_state() {
        let path = std::env::temp_dir().join(format!("loom-persistence-{}.db", Uuid::new_v4()));
        let store = FilePersistence::open(&path).unwrap();
        let session_id = AgentSessionId::new();
        let run_id = RunId::new();
        assert!(!store.exists());
        assert!(store.load_sessions().unwrap().is_none());
        assert!(store.load_workspaces().unwrap().is_none());
        assert!(store.load_run_messages(run_id).unwrap().is_empty());
        assert!(
            store
                .load_run_message_page(run_id, None, 1)
                .unwrap()
                .is_empty()
        );
        assert!(store.load_filesystem_record(session_id).unwrap().is_none());
        assert!(store.load_feed_state().unwrap().is_none());
        assert!(store.load_feed_header().unwrap().is_none());
        assert!(
            store
                .load_feed_session_cursor(session_id)
                .unwrap()
                .is_none()
        );
        assert!(store.load_feed_events_since(None, None).unwrap().is_empty());
        assert!(!store.exists());
    }

    #[test]
    fn filesystem_change_writes_retain_only_the_newest_entries() {
        let session_id = AgentSessionId::new();
        let changes = (1..=(MAX_FILESYSTEM_CHANGE_HISTORY as u64 + 1))
            .map(|sequence| SessionFilesystemChange {
                sequence: EventSequence::new(sequence),
                session_id,
                path: format!("file-{sequence}"),
                kind: WorkspaceChangeKind::Created,
                revision: None,
            })
            .collect::<Vec<_>>();
        let mut connection = Connection::open_in_memory().unwrap();
        connection
            .execute_batch(
                "CREATE TABLE filesystem_changes(
                    session_id BLOB NOT NULL, sequence INTEGER NOT NULL,
                    path TEXT NOT NULL, kind TEXT NOT NULL, revision TEXT,
                    PRIMARY KEY(session_id, sequence)
                ) WITHOUT ROWID;",
            )
            .unwrap();
        let transaction = connection.transaction().unwrap();
        save_filesystem_change_rows(&transaction, session_id, &changes).unwrap();
        let (count, min_sequence, max_sequence): (i64, i64, i64) = transaction
            .query_row(
                "SELECT COUNT(*), MIN(sequence), MAX(sequence) FROM filesystem_changes",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(count, MAX_FILESYSTEM_CHANGE_HISTORY as i64);
        assert_eq!(min_sequence, 2);
        assert_eq!(max_sequence, MAX_FILESYSTEM_CHANGE_HISTORY as i64 + 1);
    }

    #[test]
    fn database_with_user_tables_is_rejected_without_importing_or_modifying_it() {
        let path = std::env::temp_dir().join(format!("loom-persistence-{}.db", Uuid::new_v4()));
        let repeated = "large checkpoint content ".repeat(1000);
        let legacy_value = serde_json::json!({
            "sessions": [
                {"id": "first", "checkpoint": repeated},
                {"id": "second", "checkpoint": repeated}
            ],
            "next_sequence": 17
        });
        let connection = Connection::open(&path).unwrap();
        let original_journal_mode: String = connection
            .pragma_query_value(None, "journal_mode", |row| row.get(0))
            .unwrap();
        connection
            .execute_batch(
                "CREATE TABLE sections (
                    name TEXT PRIMARY KEY NOT NULL,
                    schema_version INTEGER NOT NULL,
                    payload BLOB NOT NULL
                );",
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO sections(name, schema_version, payload) VALUES ('legacy', 2, ?1)",
                [serde_json::to_vec(&legacy_value).unwrap()],
            )
            .unwrap();
        drop(connection);

        let store = FilePersistence::open(&path).unwrap();
        assert_eq!(
            store.load_feed_header().unwrap_err().code,
            ErrorCode::MalformedPayload
        );
        let connection = Connection::open(&path).unwrap();
        let journal_mode: String = connection
            .pragma_query_value(None, "journal_mode", |row| row.get(0))
            .unwrap();
        assert_eq!(journal_mode, original_journal_mode);
        let raw_payload: Vec<u8> = connection
            .query_row(
                "SELECT payload FROM sections WHERE name='legacy'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(raw_payload, serde_json::to_vec(&legacy_value).unwrap());
        let typed_schema_exists: bool = connection
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='sessions')",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(!typed_schema_exists);
        let version: u32 = connection
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .unwrap();
        assert_eq!(version, 0);
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn previous_database_version_is_rejected_without_schema_changes() {
        let path = std::env::temp_dir().join(format!("loom-persistence-{}.db", Uuid::new_v4()));
        let connection = Connection::open(&path).unwrap();
        connection
            .execute_batch("CREATE TABLE sentinel(value TEXT); PRAGMA user_version=40;")
            .unwrap();
        let original_journal_mode: String = connection
            .pragma_query_value(None, "journal_mode", |row| row.get(0))
            .unwrap();
        drop(connection);

        let store = FilePersistence::open(&path).unwrap();
        assert_eq!(
            store.load_feed_header().unwrap_err().code,
            ErrorCode::MalformedPayload
        );

        let connection = Connection::open(&path).unwrap();
        let version: u32 = connection
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .unwrap();
        let journal_mode: String = connection
            .pragma_query_value(None, "journal_mode", |row| row.get(0))
            .unwrap();
        let has_feed_meta: bool = connection
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='feed_session_meta')",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(version, 40);
        assert_eq!(journal_mode, original_journal_mode);
        assert!(!has_feed_meta);
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn future_database_version_is_rejected_without_schema_changes() {
        let path = std::env::temp_dir().join(format!("loom-persistence-{}.db", Uuid::new_v4()));
        let connection = Connection::open(&path).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE sentinel(value TEXT);
                 INSERT INTO sentinel(value) VALUES ('keep');
                 PRAGMA user_version=52;",
            )
            .unwrap();
        let original_journal_mode: String = connection
            .pragma_query_value(None, "journal_mode", |row| row.get(0))
            .unwrap();
        drop(connection);

        let store = FilePersistence::open(&path).unwrap();
        assert_eq!(
            store.load_feed_header().unwrap_err().code,
            ErrorCode::MalformedPayload
        );

        let connection = Connection::open(&path).unwrap();
        let version: u32 = connection
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .unwrap();
        let journal_mode: String = connection
            .pragma_query_value(None, "journal_mode", |row| row.get(0))
            .unwrap();
        let sentinel: String = connection
            .query_row("SELECT value FROM sentinel", [], |row| row.get(0))
            .unwrap();
        let has_feed_meta: bool = connection
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='feed_session_meta')",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(version, 52);
        assert_eq!(journal_mode, original_journal_mode);
        assert_eq!(sentinel, "keep");
        assert!(!has_feed_meta);
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn fresh_database_uses_typed_schema_without_generic_section_tables() {
        let path = std::env::temp_dir().join(format!("loom-persistence-{}.db", Uuid::new_v4()));
        let store = FilePersistence::open(&path).unwrap();
        store
            .save_state_with_sessions(&SessionManager::default().export_state())
            .unwrap();

        let connection = Connection::open(&path).unwrap();
        let generic_tables: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master
                 WHERE type='table' AND name IN ('section_meta', 'state_nodes')",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let version: u32 = connection
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .unwrap();
        assert_eq!(generic_tables, 0);
        assert_eq!(version, DATABASE_SCHEMA_VERSION);
        drop(connection);
        drop(store);
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn baseline_schema_consolidates_project_grants_into_single_json_columns() {
        let path = std::env::temp_dir().join(format!("loom-persistence-{}.db", Uuid::new_v4()));
        let store = FilePersistence::open(&path).unwrap();
        store
            .save_state_with_sessions(&SessionManager::default().export_state())
            .unwrap();

        let connection = Connection::open(&path).unwrap();
        let version: u32 = connection
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .unwrap();
        assert_eq!(version, DATABASE_SCHEMA_VERSION);

        let run_columns: Vec<String> = connection
            .prepare(
                "SELECT name FROM pragma_table_info('run_runtime_config')
                 WHERE name LIKE 'project_%' ORDER BY name",
            )
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .collect::<std::result::Result<_, _>>()
            .unwrap();
        assert_eq!(run_columns, vec!["project_grants".to_owned()]);

        let task_columns: Vec<String> = connection
            .prepare(
                "SELECT name FROM pragma_table_info('delegated_tasks')
                 WHERE name LIKE 'permission_%' OR name = 'permissions' ORDER BY name",
            )
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .collect::<std::result::Result<_, _>>()
            .unwrap();
        assert_eq!(task_columns, vec!["permissions".to_owned()]);

        drop(connection);
        drop(store);
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn baseline_schema_folds_transcript_and_tool_state_into_parent_rows() {
        let path = std::env::temp_dir().join(format!("loom-persistence-{}.db", Uuid::new_v4()));
        let store = FilePersistence::open(&path).unwrap();
        store
            .save_state_with_sessions(&SessionManager::default().export_state())
            .unwrap();

        let connection = Connection::open(&path).unwrap();
        let child_tables: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name IN (
                    'run_message_fragments', 'run_message_tool_calls', 'run_tool_attempts'
                 )",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(child_tables, 0);

        let message_columns: Vec<String> = connection
            .prepare(
                "SELECT name FROM pragma_table_info('run_messages')
                 WHERE name IN ('tool_calls', 'fragments') ORDER BY name",
            )
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .collect::<std::result::Result<_, _>>()
            .unwrap();
        assert_eq!(
            message_columns,
            vec!["fragments".to_owned(), "tool_calls".to_owned()]
        );

        let call_columns: Vec<String> = connection
            .prepare(
                "SELECT name FROM pragma_table_info('run_tool_calls')
                 WHERE name = 'attempts'",
            )
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .collect::<std::result::Result<_, _>>()
            .unwrap();
        assert_eq!(call_columns, vec!["attempts".to_owned()]);

        drop(connection);
        drop(store);
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn project_grants_json_is_forward_and_backward_compatible() {
        // Missing keys default to disabled and unknown keys are ignored, so a
        // future grant is added without a schema migration.
        let grants: RunProjectGrants = decode_json(
            r#"{"delegation":true,"future_grant":true}"#,
            "run project grants",
        )
        .unwrap();
        let mut config = DurableRunRuntimeConfig {
            system_instructions: None,
            repository_instructions: None,
            approval_policy: ApprovalPolicy::default(),
            limits: SessionLimits::default(),
            context_options: ContextAssemblyOptions::default(),
            checkpoint_id: None,
            input_cost_micros_per_1k: 0,
            output_cost_micros_per_1k: 0,
            context_inspection: None,
            project_delegation_enabled: false,
            project_messaging_enabled: true,
            project_inspection_enabled: false,
            project_child_control_enabled: false,
            project_worktree_enabled: false,
            project_review_enabled: false,
            project_integration_enabled: false,
            project_branch_messaging_enabled: false,
        };
        grants.apply(&mut config);
        assert!(config.project_delegation_enabled);
        assert!(!config.project_messaging_enabled);
        assert_eq!(
            RunProjectGrants::of(&config),
            RunProjectGrants {
                delegation: true,
                ..RunProjectGrants::default()
            }
        );

        // Round trip through the stored representation.
        let payload = serde_json::to_string(&RunProjectGrants::of(&config)).unwrap();
        let restored: RunProjectGrants = decode_json(&payload, "run project grants").unwrap();
        assert!(restored.delegation);
        assert!(!restored.messaging);
    }

    #[test]
    fn schema_status_detects_incompatible_state_and_resets_only_on_request() {
        let path = std::env::temp_dir().join(format!("loom-persistence-{}.db", Uuid::new_v4()));
        assert_eq!(
            FilePersistence::schema_status(&path).unwrap(),
            SchemaStatus::Absent
        );

        let store = FilePersistence::open(&path).unwrap();
        store
            .save_state_with_sessions(&SessionManager::default().export_state())
            .unwrap();
        assert_eq!(
            FilePersistence::schema_status(&path).unwrap(),
            SchemaStatus::Current
        );
        drop(store);

        // Simulate a database written by an older Loom revision.
        {
            let connection = Connection::open(&path).unwrap();
            connection
                .pragma_update(None, "user_version", 40u32)
                .unwrap();
        }
        assert_eq!(
            FilePersistence::schema_status(&path).unwrap(),
            SchemaStatus::OtherVersion(40)
        );

        // Inspecting and refusing to wipe must leave the file untouched.
        assert_eq!(
            prepare_database(&path, false).unwrap(),
            SchemaStatus::OtherVersion(40)
        );
        assert!(path.is_file());

        // An explicit wipe removes the database and its sidecars.
        assert_eq!(prepare_database(&path, true).unwrap(), SchemaStatus::Absent);
        assert!(!path.is_file());
        fs::remove_file(&path).ok();
    }

    #[test]
    fn small_content_is_inline_compressed_and_read_by_range() {
        let path = std::env::temp_dir().join(format!("loom-persistence-{}.db", Uuid::new_v4()));
        let text = "repeated transcript text with useful detail\n".repeat(48);
        let connection = Connection::open(&path).unwrap();
        initialize_schema(&connection).unwrap();
        let transaction = connection.unchecked_transaction().unwrap();
        let hash = store_content(&transaction, text.as_bytes()).unwrap();
        transaction.commit().unwrap();

        let (codec, payload_length, parts, blobs): (i64, i64, i64, i64) = connection
            .query_row(
                "SELECT objects.inline_codec, length(objects.inline_payload),
                        (SELECT COUNT(*) FROM content_parts WHERE content_hash=objects.hash),
                        (SELECT COUNT(*) FROM content_blobs)
                 FROM content_objects objects WHERE objects.hash=?1",
                [&hash],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .unwrap();
        assert_eq!(codec, 1);
        assert!(payload_length < text.len() as i64);
        assert_eq!((parts, blobs), (0, 0));
        assert_eq!(decode_content(&connection, &hash).unwrap(), text);
        assert_eq!(
            load_content_range(&connection, &hash, 13, 41).unwrap(),
            text.as_bytes()[13..54]
        );

        connection
            .execute(
                "UPDATE content_objects SET inline_payload=x'0102' WHERE hash=?1",
                [&hash],
            )
            .unwrap();
        assert_eq!(
            decode_content(&connection, &hash).unwrap_err().code,
            ErrorCode::MalformedPayload
        );
        drop(connection);
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn corrupt_content_objects_are_rejected_during_restore() {
        let path = std::env::temp_dir().join(format!("loom-persistence-{}.db", Uuid::new_v4()));
        let text = "content whose integrity must survive process restarts ".repeat(200);
        let connection = Connection::open(&path).unwrap();
        initialize_schema(&connection).unwrap();
        let transaction = connection.unchecked_transaction().unwrap();
        store_content(&transaction, text.as_bytes()).unwrap();
        transaction.commit().unwrap();
        let (object_hash, blob_hash): (Vec<u8>, Vec<u8>) = connection
            .query_row(
                "SELECT objects.hash, parts.blob_hash
                 FROM content_objects AS objects
                 JOIN content_parts AS parts ON parts.content_hash=objects.hash
                 LIMIT 1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();

        connection
            .execute(
                "UPDATE content_blobs SET codec=1, payload=x'0102' WHERE hash=?1",
                [&blob_hash],
            )
            .unwrap();
        let malformed_compressed = decode_content(&connection, &object_hash).unwrap_err();
        assert_eq!(malformed_compressed.code, ErrorCode::MalformedPayload);

        connection
            .execute(
                "UPDATE content_blobs SET codec=0, payload=x'00' WHERE hash=?1",
                [&blob_hash],
            )
            .unwrap();
        let damaged_content = decode_content(&connection, &object_hash).unwrap_err();
        assert_eq!(damaged_content.code, ErrorCode::MalformedPayload);

        drop(connection);
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn project_hierarchy_constraints_reject_invalid_depth_and_cross_project_parent() {
        let path = std::env::temp_dir().join(format!("loom-persistence-{}.db", Uuid::new_v4()));
        let persistence = FilePersistence::open(&path).unwrap();
        let mut manager = SessionManager::default();
        let (first, _) = manager
            .create_in_workspace(WorkspaceId::new(), "First project")
            .unwrap();
        let (second, _) = manager
            .create_in_workspace(WorkspaceId::new(), "Second project")
            .unwrap();
        persistence
            .save_state_with_sessions(&manager.export_state())
            .unwrap();

        let connection = Connection::open(&path).unwrap();
        let child = Uuid::new_v4();
        let cross_project = connection.execute(
            "INSERT INTO sessions_hierarchy(project_id, session_id, parent_session_id, depth)
             VALUES (?1, ?2, ?3, 2)",
            params![
                first.id.as_uuid().as_bytes().as_slice(),
                child.as_bytes().as_slice(),
                second.id.as_uuid().as_bytes().as_slice()
            ],
        );
        assert!(cross_project.is_err());

        let too_deep = connection.execute(
            "INSERT INTO sessions_hierarchy(project_id, session_id, parent_session_id, depth)
             VALUES (?1, ?2, ?3, 4)",
            params![
                first.id.as_uuid().as_bytes().as_slice(),
                child.as_bytes().as_slice(),
                first.id.as_uuid().as_bytes().as_slice()
            ],
        );
        assert!(too_deep.is_err());
        drop(connection);
        drop(persistence);
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn project_child_and_agent_messages_are_atomic_ordered_and_request_idempotent() {
        let path = std::env::temp_dir().join(format!("loom-persistence-{}.db", Uuid::new_v4()));
        let persistence = FilePersistence::open(&path).unwrap();
        let mut manager = SessionManager::default();
        let (root, _) = manager
            .create_in_workspace(WorkspaceId::new(), "Project manager")
            .unwrap();
        persistence
            .save_state_with_sessions(&manager.export_state())
            .unwrap();

        let child_id = AgentSessionId::new();
        let child_snapshot = AgentSessionSnapshot {
            id: child_id,
            workspace_id: root.workspace_id,
            name: "Research agent".to_owned(),
            state: AgentSessionState::Idle,
            created_at: Timestamp::now(),
            updated_at: Timestamp::now(),
        };
        let task = DelegatedTaskRecord {
            task_id: TaskId::new(),
            project_id: ProjectId::from_uuid(*root.id.as_uuid()),
            requester_session_id: root.id,
            target_session_id: child_id,
            child_name: child_snapshot.name.clone(),
            intent: "Inspect the relevant module".to_owned(),
            model_id: "deterministic/demo".to_owned(),
            context_references: vec![TaskContextReference {
                label: "architecture".to_owned(),
                uri: "docs/architecture.md".to_owned(),
            }],
            dependencies: Vec::new(),
            code_change: true,
            permissions: ProjectAgentPermissions {
                delegation: false,
                branch_messaging: true,
                child_control: false,
                inspection: true,
                worktree_creation: true,
                review: true,
                integration: false,
            },
            status: DelegatedTaskStatus::Queued,
            created_at: child_snapshot.created_at,
            updated_at: child_snapshot.updated_at,
        };
        let parent_repository_id = RepositoryId::new();
        let child_repository_id = RepositoryId::new();
        let initial_worktree = ProjectWorktreeRecord {
            project_id: task.project_id,
            task_id: task.task_id,
            parent_session_id: root.id,
            child_session_id: child_id,
            parent_repository_id,
            child_repository_id,
            relative_path: "worktrees/task-a".to_owned(),
            worktree_name: "task-a".to_owned(),
            branch_name: "codex/task-a".to_owned(),
            base_revision: "base-sha".to_owned(),
            result_revision: None,
            integrated_revision: None,
            status: ProjectWorktreeStatus::Creating,
            conflict_paths: Vec::new(),
            error: None,
            cleanup_disposition: None,
            created_at: child_snapshot.created_at,
            updated_at: child_snapshot.updated_at,
        };
        let request_id = RequestId::new();
        let created = persistence
            .create_project_child_with_worktree(
                request_id,
                &child_snapshot,
                manager.export_state().next_sequence,
                &task,
                &initial_worktree,
            )
            .unwrap();
        assert_eq!(created, task);
        let worktree = ProjectWorktreeRecord {
            result_revision: Some("result-sha".to_owned()),
            integrated_revision: None,
            status: ProjectWorktreeStatus::Conflict,
            conflict_paths: vec!["src/main.rs".to_owned(), "docs/plan.md".to_owned()],
            error: Some("integration found conflicts".to_owned()),
            cleanup_disposition: Some(ProjectWorktreeCleanupDisposition::Retain),
            updated_at: Timestamp::now(),
            ..initial_worktree.clone()
        };
        persistence.save_project_worktree(&worktree).unwrap();
        assert_eq!(
            persistence
                .load_project_worktree_by_task(task.task_id)
                .unwrap(),
            Some(worktree.clone())
        );
        let project_snapshot = persistence
            .load_project_snapshot(task.project_id)
            .unwrap()
            .unwrap();
        assert_eq!(project_snapshot.worktrees, vec![worktree]);
        let child_projection = project_snapshot
            .agents
            .iter()
            .find(|agent| agent.session_id == child_id)
            .unwrap();
        assert_eq!(
            child_projection.task_summary.as_deref(),
            Some(task.intent.as_str())
        );
        assert_eq!(
            persistence.load_delegated_task(task.task_id).unwrap(),
            Some(task.clone())
        );
        assert_eq!(
            persistence.list_project_tasks(task.project_id).unwrap(),
            vec![task.clone()]
        );
        let task_spec = DelegatedTaskSpec {
            intent: task.intent.clone(),
            model_id: task.model_id.clone(),
            context_references: task.context_references.clone(),
            dependencies: task.dependencies.clone(),
            code_change: task.code_change,
            permissions: task.permissions,
        };
        assert_eq!(
            persistence
                .load_project_child_by_request(
                    request_id,
                    task.project_id,
                    task.requester_session_id,
                    &task.child_name,
                    &task_spec,
                )
                .unwrap(),
            Some(task.clone())
        );
        assert!(
            persistence
                .load_project_child_by_request(
                    request_id,
                    task.project_id,
                    task.requester_session_id,
                    "different child name",
                    &task_spec,
                )
                .is_err()
        );

        // Retrying the atomic code-task creation must preserve its worktree
        // intent identity even after the worktree has advanced to conflict.
        assert_eq!(
            persistence
                .create_project_child_with_worktree(
                    request_id,
                    &child_snapshot,
                    manager.export_state().next_sequence,
                    &task,
                    &initial_worktree,
                )
                .unwrap(),
            task
        );
        assert!(
            persistence
                .update_delegated_task_status(
                    task.task_id,
                    DelegatedTaskStatus::Running,
                    Timestamp::now()
                )
                .unwrap()
        );
        assert_eq!(
            persistence
                .load_delegated_task(task.task_id)
                .unwrap()
                .unwrap()
                .status,
            DelegatedTaskStatus::Running
        );
        assert!(
            !persistence
                .update_delegated_task_status_if_queued(
                    task.task_id,
                    DelegatedTaskStatus::Blocked,
                    Timestamp::now(),
                )
                .unwrap(),
            "a scheduler claim must not overwrite a status advanced by runtime recovery"
        );

        let draft = AgentMessageDraft {
            project_id: task.project_id,
            task_id: Some(task.task_id),
            sender_session_id: root.id,
            target_session_id: child_id,
            kind: AgentMessageKind::Direction,
            body: "Start with the persistence layer".to_owned(),
        };
        let message_request = RequestId::new();
        let accepted = persistence
            .accept_agent_message(message_request, &draft)
            .unwrap();
        assert_eq!(accepted.project_sequence, 1);
        assert_eq!(accepted.kind, AgentMessageKind::Direction);
        assert_eq!(
            persistence
                .accept_agent_message(message_request, &draft)
                .unwrap(),
            accepted
        );
        let next = persistence
            .accept_agent_message(
                RequestId::new(),
                &AgentMessageDraft {
                    target_session_id: root.id,
                    body: "Child completed a first pass".to_owned(),
                    kind: AgentMessageKind::Progress,
                    ..draft.clone()
                },
            )
            .unwrap();
        assert_eq!(next.project_sequence, 2);
        assert_eq!(
            persistence
                .list_agent_messages(task.project_id, child_id, 0, 10)
                .unwrap(),
            vec![accepted]
        );

        let mismatch = AgentMessageDraft {
            body: "changed payload".to_owned(),
            ..draft
        };
        assert!(
            persistence
                .accept_agent_message(message_request, &mismatch)
                .is_err()
        );
        drop(persistence);
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn project_cancellation_cascade_intent_is_ordered_idempotent_and_removable() {
        let path = std::env::temp_dir().join(format!("loom-persistence-{}.db", Uuid::new_v4()));
        let persistence = FilePersistence::open(&path).unwrap();
        let mut manager = SessionManager::default();
        let (root, _) = manager
            .create_in_workspace(WorkspaceId::new(), "Project manager")
            .unwrap();
        persistence
            .save_state_with_sessions(&manager.export_state())
            .unwrap();
        let child_id = AgentSessionId::new();
        let child_snapshot = AgentSessionSnapshot {
            id: child_id,
            workspace_id: root.workspace_id,
            name: "Research agent".to_owned(),
            state: AgentSessionState::Idle,
            created_at: Timestamp::now(),
            updated_at: Timestamp::now(),
        };
        let task = DelegatedTaskRecord {
            task_id: TaskId::new(),
            project_id: ProjectId::from_uuid(*root.id.as_uuid()),
            requester_session_id: root.id,
            target_session_id: child_id,
            child_name: child_snapshot.name.clone(),
            intent: "Inspect the relevant module".to_owned(),
            model_id: "deterministic/demo".to_owned(),
            context_references: Vec::new(),
            dependencies: Vec::new(),
            code_change: false,
            permissions: ProjectAgentPermissions::default(),
            status: DelegatedTaskStatus::Queued,
            created_at: child_snapshot.created_at,
            updated_at: child_snapshot.updated_at,
        };
        persistence
            .create_project_child(
                RequestId::new(),
                &child_snapshot,
                manager.export_state().next_sequence,
                &task,
            )
            .unwrap();
        let cascade = ProjectCancellationCascadeRecord {
            project_id: task.project_id,
            root_task_id: task.task_id,
            manager_session_id: root.id,
            members: vec![(task.task_id, child_id)],
            created_at: Timestamp::now(),
        };
        assert_eq!(
            persistence
                .begin_project_cancellation_cascade(&cascade)
                .unwrap(),
            cascade
        );
        assert!(
            persistence
                .has_pending_project_cancellation_cascade(task.project_id)
                .unwrap()
        );
        assert_eq!(
            persistence
                .list_pending_project_cancellation_cascades()
                .unwrap(),
            vec![cascade.clone()]
        );
        assert_eq!(
            persistence
                .begin_project_cancellation_cascade(&cascade)
                .unwrap(),
            cascade
        );
        let invalid = ProjectCancellationCascadeRecord {
            members: Vec::new(),
            ..cascade.clone()
        };
        assert!(
            persistence
                .begin_project_cancellation_cascade(&invalid)
                .is_err()
        );
        assert!(
            persistence
                .complete_project_cancellation_cascade(task.project_id, task.task_id)
                .unwrap()
        );
        assert!(
            !persistence
                .has_pending_project_cancellation_cascade(task.project_id)
                .unwrap()
        );
        assert!(
            persistence
                .list_pending_project_cancellation_cascades()
                .unwrap()
                .is_empty()
        );
        assert!(
            !persistence
                .complete_project_cancellation_cascade(task.project_id, task.task_id)
                .unwrap()
        );
        drop(persistence);
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn typed_session_catalog_round_trips_and_uses_the_picker_index() {
        let path = std::env::temp_dir().join(format!("loom-persistence-{}.db", Uuid::new_v4()));
        let persistence = FilePersistence::open(&path).unwrap();
        let mut manager = SessionManager::default();
        let (first, _) = manager
            .create_in_workspace(WorkspaceId::new(), "First session")
            .unwrap();
        let (second, _) = manager
            .create_in_workspace(first.workspace_id, "Second session")
            .unwrap();
        manager.archive(first.id).unwrap();
        let state = manager.export_state();

        persistence.save_state_with_sessions(&state).unwrap();
        let connection = Connection::open(&path).unwrap();
        let root_rows: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM sessions_hierarchy
                 WHERE project_id=session_id AND parent_session_id IS NULL AND depth=1",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(root_rows, state.sessions.len() as i64);
        let first_project = persistence
            .load_project_snapshot(ProjectId::from_uuid(*first.id.as_uuid()))
            .unwrap()
            .unwrap();
        assert_eq!(first_project.root_session_id, first.id);
        assert_eq!(first_project.agents.len(), 1);
        assert_eq!(first_project.agents[0].session_id, first.id);
        assert_eq!(first_project.agents[0].project_id, first_project.project_id);
        assert_eq!(first_project.agents[0].depth, 1);
        assert_eq!(first_project.agents[0].parent_session_id, None);
        let restored = persistence.load_sessions().unwrap().unwrap();
        assert_eq!(restored, state);
        assert_eq!(
            SessionManager::from_state(restored)
                .unwrap()
                .get(second.id)
                .unwrap(),
            second
        );

        let plan: String = connection
            .query_row(
                "EXPLAIN QUERY PLAN
                 SELECT id, name, updated_at FROM sessions
                 WHERE workspace_id = ?1 AND state != 'archived'
                 ORDER BY updated_at DESC, id DESC LIMIT 20",
                [second.workspace_id.as_uuid().as_bytes().as_slice()],
                |row| row.get(3),
            )
            .unwrap();
        assert!(plan.contains("sessions_visible"), "{plan}");
        connection
            .execute_batch(
                "CREATE TABLE session_updates(count INTEGER NOT NULL);
                 INSERT INTO session_updates VALUES (0);
                 CREATE TRIGGER track_session_updates AFTER UPDATE ON sessions BEGIN
                    UPDATE session_updates SET count=count+1;
                 END;",
            )
            .unwrap();
        drop(connection);
        persistence.save_state_with_sessions(&state).unwrap();
        let connection = Connection::open(&path).unwrap();
        let updates: i64 = connection
            .query_row("SELECT count FROM session_updates", [], |row| row.get(0))
            .unwrap();
        assert_eq!(updates, 0, "unchanged rows must not be rewritten");
        drop(connection);
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn typed_workspace_catalog_round_trips_and_uses_its_activity_index() {
        let path = std::env::temp_dir().join(format!("loom-persistence-{}.db", Uuid::new_v4()));
        let persistence = FilePersistence::open(&path).unwrap();
        let mut manager = WorkspaceManager::default();
        let first = manager.create("First workspace").unwrap();
        let second = manager.create("Second workspace").unwrap();
        let state = manager.export_state();

        persistence
            .save_state_with_catalogs_and_feed(
                &SessionManager::default().export_state(),
                &state,
                None,
            )
            .unwrap();
        assert_eq!(persistence.load_workspaces().unwrap().unwrap(), state);

        let connection = Connection::open(&path).unwrap();
        let plan: String = connection
            .query_row(
                "EXPLAIN QUERY PLAN SELECT id, name, updated_at FROM workspaces
                 ORDER BY updated_at DESC, id DESC LIMIT 20",
                [],
                |row| row.get(3),
            )
            .unwrap();
        assert!(plan.contains("workspaces_by_activity"), "{plan}");
        connection
            .execute_batch(
                "CREATE TABLE workspace_updates(count INTEGER NOT NULL);
                 INSERT INTO workspace_updates VALUES (0);
                 CREATE TRIGGER track_workspace_updates AFTER UPDATE ON workspaces BEGIN
                    UPDATE workspace_updates SET count=count+1;
                 END;",
            )
            .unwrap();
        drop(connection);
        persistence
            .save_state_with_catalogs_and_feed(
                &SessionManager::default().export_state(),
                &state,
                None,
            )
            .unwrap();
        let connection = Connection::open(&path).unwrap();
        let updates: i64 = connection
            .query_row("SELECT count FROM workspace_updates", [], |row| row.get(0))
            .unwrap();
        assert_eq!(updates, 0, "unchanged workspace rows must not be rewritten");
        assert_ne!(first.id, second.id);
        drop(connection);
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn session_and_workspace_settings_are_bounded_indexed_and_atomic() {
        let path = std::env::temp_dir().join(format!("loom-persistence-{}.db", Uuid::new_v4()));
        let persistence = FilePersistence::open(&path).unwrap();
        let mut sessions = SessionManager::default();
        let (session, _) = sessions
            .create_in_workspace(WorkspaceId::new(), "Settings owner")
            .unwrap();
        let mut workspaces = WorkspaceManager::default();
        let workspace = workspaces.create("Configured workspace").unwrap();
        let settings = DurableSessionSettings {
            approval_policies: BTreeMap::from([(session.id, ApprovalPolicy::default())]),
            auto_approve_actions: BTreeMap::from([(session.id, true)]),
        };
        let config = WorkspaceConfig {
            revision: 4,
            ..WorkspaceConfig::default()
        };
        let configs = BTreeMap::from([(workspace.id, config.clone())]);
        let provider_config = ProviderConfig::deterministic();
        let provider_id = provider_config.id.clone();
        let provider_state = DurableProviderState {
            configs: BTreeMap::from([(provider_id.clone(), provider_config.clone())]),
            health: BTreeMap::from([(provider_id.clone(), ProviderHealth::default())]),
        };
        let model_id = ModelId::new("deterministic-model");
        let mut usage = UsageLedger::default();
        usage.record(
            provider_id.clone(),
            model_id.clone(),
            loom_model::TokenUsage {
                input_tokens: 3,
                output_tokens: 5,
                cached_input_tokens: 1,
            },
            17,
        );
        usage.record(
            provider_id.clone(),
            model_id.clone(),
            loom_model::TokenUsage {
                input_tokens: 4,
                output_tokens: 2,
                cached_input_tokens: 0,
            },
            9,
        );
        let request_id = RequestId::new();
        let idempotency = BTreeMap::from([(
            request_id,
            DurableIdempotencyRecord {
                created_at: Timestamp::from_unix_millis(1234),
                expires_at: Some(Timestamp::from_unix_millis(604_801_234)),
                request: serde_json::json!({"method": "list_sessions"}),
                response: serde_json::json!({"sessions": []}),
            },
        )]);
        let run_id = RunId::new();
        let run_summary = DurableRunSummary {
            snapshot: AgentRunSnapshot {
                id: run_id,
                attempt_id: loom_core::RunAttemptId::new(),
                control_revision: 0,
                session_id: session.id,
                task: "indexed run summary".to_owned(),
                model: ModelId::new("deterministic-model"),
                state: AgentRunState::Completed,
                started_at: Timestamp::from_unix_millis(1000),
                updated_at: Timestamp::from_unix_millis(2000),
                completed_at: Some(Timestamp::from_unix_millis(2000)),
                summary: Some("finished".to_owned()),
                evidence: vec![loom_core::EvidenceLink {
                    label: "Build output".to_owned(),
                    uri: "file:///workspace/build.log".to_owned(),
                }],
            },
            usage: UsageSnapshot {
                input_tokens: 13,
                output_tokens: 7,
                ..UsageSnapshot::default()
            },
            attempts: None,
            execution_state: None,
            interactions: None,
        };
        let second_run_id = RunId::new();
        let mut second_summary = run_summary.clone();
        second_summary.snapshot.id = second_run_id;
        second_summary.snapshot.attempt_id = loom_core::RunAttemptId::new();
        second_summary.usage = UsageSnapshot::default();
        let run_summaries =
            BTreeMap::from([(run_id, run_summary), (second_run_id, second_summary)]);
        let run_runtime_config = DurableRunRuntimeConfig {
            system_instructions: Some("Use the project conventions".to_owned()),
            repository_instructions: Some("Do not modify generated files".to_owned()),
            approval_policy: ApprovalPolicy {
                write: PolicyDecision::Deny,
                ..ApprovalPolicy::default()
            },
            limits: SessionLimits {
                max_duration_ms: Some(60_000),
                max_input_tokens: Some(12_000),
                max_output_tokens: Some(3_000),
                max_tool_calls: Some(12),
                max_cost_micros: Some(250_000),
            },
            context_options: ContextAssemblyOptions {
                context_window: Some(16_000),
                max_input_tokens: Some(12_000),
                reserved_output_tokens: Some(3_000),
            },
            checkpoint_id: Some(CheckpointId::new()),
            input_cost_micros_per_1k: 17,
            output_cost_micros_per_1k: 29,
            context_inspection: None,
            project_delegation_enabled: true,
            project_messaging_enabled: true,
            project_inspection_enabled: true,
            project_child_control_enabled: true,
            project_worktree_enabled: true,
            project_review_enabled: false,
            project_integration_enabled: false,
            project_branch_messaging_enabled: true,
        };
        let run_runtime_configs = BTreeMap::from([
            (run_id, run_runtime_config.clone()),
            (second_run_id, run_runtime_config.clone()),
        ]);
        let context_checkpoint = DurableRunContextCheckpoint {
            session_id: session.id,
            summary: ContextSummary {
                text: "older conversation summary".to_owned(),
                source_message_count: 12,
                projection_version: 1,
                source_digest: "ab".repeat(32),
                created_at: Timestamp::from_unix_millis(2100),
            },
        };
        let run_context_checkpoints = BTreeMap::from([(run_id, Some(context_checkpoint.clone()))]);
        let run_plans = BTreeMap::from([(
            run_id,
            AgentPlan {
                steps: vec![AgentPlanStep {
                    id: "inspect".to_owned(),
                    description: "Inspect the relevant source and build output".to_owned(),
                }],
            },
        )]);
        let activity_call = loom_model::ToolCall {
            id: loom_core::ToolCallId::new(),
            name: "read_file".to_owned(),
            arguments: serde_json::json!({"path": "src/main.rs"}),
        };
        let activity = AgentActivityRecord {
            id: ActivityId::new(),
            run_id,
            timeline_ordinal: 0,
            parent_id: None,
            step_id: Some(StepId::new()),
            kind: AgentActivityKind::ToolCall,
            status: AgentActivityStatus::AwaitingApproval,
            started_at: Timestamp::from_unix_millis(1500),
            completed_at: None,
            elapsed_ms: None,
            data: AgentActivityData::ToolCall {
                call: activity_call.clone(),
                result: None,
            },
        };
        let run_activities = BTreeMap::from([(run_id, vec![activity.clone()])]);
        let run_messages = BTreeMap::from([(
            run_id,
            vec![
                DurableRunMessage {
                    timeline_ordinal: 0,
                    role: loom_model::MessageRole::User,
                    content: "large transcript content ".repeat(500),
                    name: None,
                    tool_call_id: None,
                    tool_calls: Vec::new(),
                },
                DurableRunMessage {
                    timeline_ordinal: 0,
                    role: loom_model::MessageRole::Assistant,
                    content: String::new(),
                    name: Some("assistant".to_owned()),
                    tool_call_id: None,
                    tool_calls: vec![loom_model::ToolCall {
                        id: loom_core::ToolCallId::new(),
                        name: "inspect".to_owned(),
                        arguments: serde_json::json!({"path": "src/main.rs"}),
                    }],
                },
            ],
        )]);
        let checkpoint_id = CheckpointId::new();
        let checkpoint = Checkpoint {
            id: checkpoint_id,
            session_id: session.id,
            label: "rollback point".to_owned(),
            created_at: Timestamp::from_unix_millis(4321),
            files: BTreeMap::from([(
                "src/main.rs".to_owned(),
                CheckpointFile {
                    existed: true,
                    content: "checkpoint text ".repeat(500),
                    revision: "revision-a".to_owned(),
                    expected_revision: "revision-b".to_owned(),
                },
            )]),
        };
        let repository_id = RepositoryId::new();
        let repositories = BTreeMap::from([(
            repository_id,
            SessionRepository {
                id: repository_id,
                source: "https://example.test/repo.git".to_owned(),
                path: "repositories/example".to_owned(),
                revision: Some("abc123".to_owned()),
                attached_at: Timestamp::from_unix_millis(4322),
            },
        )]);
        let directories = vec![SessionDirectory {
            source: "/tmp/external-docs".to_owned(),
            path: "docs".to_owned(),
        }];
        let filesystem_records = [DurableFilesystemRecord {
            session_id: session.id,
            root: "/tmp/loom-session-fs".to_owned(),
            control: WorkspaceControl::Agent,
            checkpoints: vec![checkpoint.clone()],
            edits: vec![DurableFilesystemEdit {
                id: 1,
                path: "src/main.rs".to_owned(),
                before: Some("before contents".to_owned()),
                before_bytes: Some(b"before contents".to_vec()),
                after_revision: "revision-after".to_owned(),
                source: WorkspaceControl::Agent,
            }],
            changes: vec![SessionFilesystemChange {
                sequence: EventSequence::new(1),
                session_id: session.id,
                path: "src/main.rs".to_owned(),
                kind: WorkspaceChangeKind::Modified,
                revision: Some("revision-after".to_owned()),
            }],
            repositories: repositories.clone(),
            directories: directories.clone(),
            payload: serde_json::json!({
                "filesystem": {
                    "session_id": session.id,
                    "root": "/tmp/loom-session-fs",
                    "control": "agent",
                    "checkpoints": [],
                    "edits": [],
                    "next_sequence": 1,
                    "changes": []
                },
                "details": "checkpoint state ".repeat(500)
            }),
            delta: None,
        }];
        persistence
            .save_state(DurableStateWrite {
                sessions: &sessions.export_state(),
                workspaces: Some(&workspaces.export_state()),
                settings: Some(&settings),
                workspace_configs: Some(&configs),
                providers: Some(&provider_state),
                usage: Some(&usage),
                idempotency: Some(&idempotency),
                run_summaries: Some(&run_summaries),
                run_runtime_configs: Some(&run_runtime_configs),
                run_context_checkpoints: Some(&run_context_checkpoints),
                run_plans: Some(&run_plans),
                run_messages: Some(&run_messages),
                run_activities: Some(&run_activities),
                filesystem_records: Some(&filesystem_records),
                feed: None,
            })
            .unwrap();
        assert_eq!(
            persistence
                .load_session_settings()
                .unwrap()
                .approval_policies,
            settings.approval_policies
        );
        assert_eq!(
            persistence
                .load_session_settings()
                .unwrap()
                .auto_approve_actions,
            settings.auto_approve_actions
        );
        assert_eq!(persistence.load_workspace_configs().unwrap(), configs);
        assert_eq!(
            persistence.load_run_runtime_config(run_id).unwrap(),
            Some(run_runtime_config.clone())
        );
        assert_eq!(
            persistence.load_provider_configs().unwrap(),
            vec![provider_config]
        );
        assert_eq!(
            persistence.load_provider_health().unwrap(),
            provider_state.health
        );
        assert_eq!(persistence.load_provider_usage().unwrap(), usage);
        assert_eq!(
            persistence.load_run_activities(run_id).unwrap(),
            vec![activity.clone()]
        );
        assert_eq!(
            persistence.load_run_tool_calls(run_id).unwrap(),
            vec![AgentToolCallRecord {
                run_id,
                session_id: session.id,
                call: activity_call.clone(),
                created_at: Timestamp::from_unix_millis(1500),
            }]
        );
        assert_eq!(
            persistence.load_run_tool_attempts(run_id).unwrap(),
            vec![AgentToolAttemptRecord {
                run_id,
                session_id: session.id,
                id: activity.id,
                call_id: activity_call.id,
                attempt_number: 1,
                state: AgentToolAttemptState::AwaitingApproval,
                started_at: Timestamp::from_unix_millis(1500),
                completed_at: None,
                result: None,
            }]
        );
        let mut completed_activity = activity.clone();
        completed_activity.status = AgentActivityStatus::Completed;
        completed_activity.completed_at = Some(Timestamp::from_unix_millis(1600));
        completed_activity.elapsed_ms = Some(100);
        let tool_result = ToolResult {
            tool_call_id: activity_call.id,
            name: activity_call.name.clone(),
            success: true,
            output: "read result".to_owned(),
        };
        completed_activity.data = AgentActivityData::ToolCall {
            call: activity_call.clone(),
            result: Some(tool_result.clone()),
        };
        let completed_activities = BTreeMap::from([(run_id, vec![completed_activity])]);
        let invalid_feed = invalid_feed_for_rollback();
        assert!(
            persistence
                .save_state(DurableStateWrite {
                    sessions: &sessions.export_state(),
                    workspaces: None,
                    settings: None,
                    workspace_configs: None,
                    providers: None,
                    usage: None,
                    idempotency: None,
                    run_summaries: None,
                    run_runtime_configs: None,
                    run_context_checkpoints: None,
                    run_plans: None,
                    run_messages: None,
                    run_activities: Some(&completed_activities),
                    filesystem_records: None,
                    feed: Some(&invalid_feed),
                })
                .is_err()
        );
        assert_eq!(
            persistence.load_run_activities(run_id).unwrap(),
            vec![activity.clone()]
        );
        assert_eq!(
            persistence.load_run_tool_attempts(run_id).unwrap()[0].state,
            AgentToolAttemptState::AwaitingApproval
        );
        persistence
            .save_state(DurableStateWrite {
                sessions: &sessions.export_state(),
                workspaces: None,
                settings: None,
                workspace_configs: None,
                providers: None,
                usage: None,
                idempotency: None,
                run_summaries: None,
                run_runtime_configs: None,
                run_context_checkpoints: None,
                run_plans: None,
                run_messages: None,
                run_activities: Some(&completed_activities),
                filesystem_records: None,
                feed: None,
            })
            .unwrap();
        assert_eq!(
            persistence.load_run_tool_attempts(run_id).unwrap(),
            vec![AgentToolAttemptRecord {
                run_id,
                session_id: session.id,
                id: activity.id,
                call_id: activity_call.id,
                attempt_number: 1,
                state: AgentToolAttemptState::Completed,
                started_at: Timestamp::from_unix_millis(1500),
                completed_at: Some(Timestamp::from_unix_millis(1600)),
                result: Some(tool_result),
            }]
        );
        let loaded_idempotency = persistence.load_idempotency_records().unwrap();
        assert_eq!(loaded_idempotency.len(), 1);
        let loaded_record = &loaded_idempotency[&request_id];
        assert_eq!(loaded_record.created_at, Timestamp::from_unix_millis(1234));
        assert_eq!(
            loaded_record.expires_at,
            Some(Timestamp::from_unix_millis(604_801_234))
        );
        assert_eq!(
            loaded_record.request,
            serde_json::json!({"method": "list_sessions"})
        );
        assert_eq!(loaded_record.response, serde_json::json!({"sessions": []}));
        assert_eq!(persistence.load_run_summaries().unwrap(), run_summaries);
        assert_eq!(
            persistence.load_run_plan(run_id).unwrap(),
            run_plans[&run_id]
        );
        assert_eq!(
            persistence
                .load_session_usage(session.id, &BTreeSet::new())
                .unwrap(),
            run_summaries[&run_id].usage
        );
        assert_eq!(
            persistence
                .load_session_usage(session.id, &BTreeSet::from([run_id]))
                .unwrap(),
            UsageSnapshot::default()
        );
        assert_eq!(
            persistence
                .load_active_run_summaries()
                .unwrap()
                .keys()
                .copied()
                .collect::<Vec<_>>(),
            run_summaries
                .iter()
                .filter(|(_, summary)| !matches!(
                    summary.snapshot.state,
                    AgentRunState::Completed | AgentRunState::Failed | AgentRunState::Cancelled
                ))
                .map(|(run_id, _)| *run_id)
                .collect::<Vec<_>>()
        );
        assert_eq!(
            persistence.load_run_summary(run_id).unwrap(),
            run_summaries.get(&run_id).cloned()
        );
        assert_eq!(
            persistence.load_run_runtime_config(run_id).unwrap(),
            Some(run_runtime_config)
        );
        assert_eq!(
            persistence.load_run_runtime_config(second_run_id).unwrap(),
            Some(run_runtime_configs[&run_id].clone())
        );
        let connection = persistence.connection().unwrap();
        let (stored_tool_limit, stored_policy, limit_storage_type): (i64, String, String) =
            connection
                .query_row(
                    "SELECT max_tool_calls, policy_write, typeof(max_tool_calls)
                     FROM run_runtime_config JOIN runtime_configurations USING(configuration_hash)
                     WHERE run_id=?1",
                    [run_id.as_uuid().as_bytes().as_slice()],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                )
                .unwrap();
        assert_eq!(stored_tool_limit, 12);
        assert_eq!(stored_policy, "deny");
        assert_eq!(limit_storage_type, "integer");
        let runtime_config_columns = connection
            .prepare("PRAGMA table_info(runtime_configurations)")
            .unwrap()
            .query_map([], |row| row.get::<_, String>(1))
            .unwrap()
            .collect::<std::result::Result<BTreeSet<_>, _>>()
            .unwrap();
        assert!(!runtime_config_columns.contains("options"));
        assert!(!runtime_config_columns.contains("approval_policy"));
        let profile_count: i64 = connection
            .query_row("SELECT COUNT(*) FROM runtime_configurations", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(
            profile_count, 1,
            "identical runtime configurations share one profile"
        );
        drop(connection);
        let mut changed_runtime_config = run_runtime_configs[&run_id].clone();
        changed_runtime_config.approval_policy.write = PolicyDecision::Allow;
        changed_runtime_config.project_branch_messaging_enabled = false;
        let mut changed_configs = BTreeMap::from([(run_id, changed_runtime_config.clone())]);
        let mut connection = Connection::open(&path).unwrap();
        connection.execute_batch("PRAGMA foreign_keys=ON;").unwrap();
        let transaction = connection.transaction().unwrap();
        save_run_runtime_config_rows(&transaction, &changed_configs).unwrap();
        transaction.commit().unwrap();
        let remaining_profiles: i64 = connection
            .query_row("SELECT COUNT(*) FROM runtime_configurations", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(
            remaining_profiles, 2,
            "the shared old profile remains in use"
        );
        drop(connection);
        assert_eq!(
            persistence.load_run_runtime_config(run_id).unwrap(),
            Some(changed_runtime_config.clone())
        );
        assert_eq!(
            persistence.load_run_runtime_config(second_run_id).unwrap(),
            Some(run_runtime_configs[&run_id].clone())
        );

        changed_configs.insert(second_run_id, changed_runtime_config.clone());
        let mut connection = Connection::open(&path).unwrap();
        connection.execute_batch("PRAGMA foreign_keys=ON;").unwrap();
        let transaction = connection.transaction().unwrap();
        save_run_runtime_config_rows(&transaction, &changed_configs).unwrap();
        transaction.commit().unwrap();
        let remaining_profiles: i64 = connection
            .query_row("SELECT COUNT(*) FROM runtime_configurations", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(remaining_profiles, 1, "the unused profile is collected");
        drop(connection);
        assert_eq!(
            persistence.load_run_runtime_config(second_run_id).unwrap(),
            Some(changed_runtime_config)
        );
        assert_eq!(
            persistence.load_run_context_checkpoint(run_id).unwrap(),
            Some(context_checkpoint.clone())
        );
        assert_eq!(
            persistence
                .load_latest_run_summary_for_session(session.id)
                .unwrap()
                .as_ref()
                .map(|summary| summary.snapshot.session_id),
            Some(session.id)
        );
        assert_eq!(
            persistence
                .load_run_summaries_for_session(session.id)
                .unwrap()
                .len(),
            run_summaries
                .values()
                .filter(|summary| summary.snapshot.session_id == session.id)
                .count()
        );
        assert_eq!(
            persistence.load_run_messages(run_id).unwrap(),
            run_messages[&run_id]
        );
        let newest_page = persistence.load_run_message_page(run_id, None, 1).unwrap();
        assert_eq!(newest_page.len(), 1);
        assert_eq!(newest_page[0].ordinal, 1);
        assert_eq!(
            newest_page[0].tool_calls,
            run_messages[&run_id][1].tool_calls
        );
        assert_eq!(
            persistence.list_filesystem_sessions().unwrap(),
            vec![session.id]
        );
        let loaded_filesystem = persistence
            .load_filesystem_record(session.id)
            .unwrap()
            .unwrap();
        assert_eq!(loaded_filesystem.payload, filesystem_records[0].payload);
        assert_eq!(loaded_filesystem.edits, filesystem_records[0].edits);
        assert!(loaded_filesystem.changes.is_empty());
        assert_eq!(
            persistence
                .load_filesystem_changes_page(session.id, None, 512)
                .unwrap()
                .changes,
            filesystem_records[0].changes
        );
        let watcher_change = SessionFilesystemChange {
            sequence: EventSequence::new(2),
            session_id: session.id,
            path: "src/generated.rs".to_owned(),
            kind: WorkspaceChangeKind::Created,
            revision: Some("revision-new".to_owned()),
        };
        persistence
            .save_filesystem_changes(
                session.id,
                EventSequence::new(2),
                std::slice::from_ref(&watcher_change),
            )
            .unwrap();
        let reloaded_filesystem = persistence
            .load_filesystem_record(session.id)
            .unwrap()
            .unwrap();
        assert_eq!(
            reloaded_filesystem.payload["filesystem"]["next_sequence"],
            serde_json::json!(2)
        );
        assert_eq!(
            persistence
                .load_filesystem_changes_page(session.id, Some(EventSequence::new(1)), 512)
                .unwrap()
                .changes,
            vec![watcher_change]
        );
        let invalid_change = SessionFilesystemChange {
            sequence: EventSequence::new(3),
            session_id: AgentSessionId::new(),
            path: "wrong-session.txt".to_owned(),
            kind: WorkspaceChangeKind::Created,
            revision: None,
        };
        assert!(
            persistence
                .save_filesystem_changes(session.id, EventSequence::new(3), &[invalid_change],)
                .is_err()
        );
        assert_eq!(
            persistence
                .load_filesystem_record(session.id)
                .unwrap()
                .unwrap()
                .payload["filesystem"]["next_sequence"],
            serde_json::json!(2),
            "sequence high-water and change rows commit atomically"
        );
        assert_eq!(loaded_filesystem.repositories, repositories);
        assert_eq!(loaded_filesystem.directories, directories);
        assert_eq!(loaded_filesystem.checkpoints, vec![checkpoint.clone()]);
        assert!(loaded_filesystem.payload.get("repositories").is_none());
        assert!(loaded_filesystem.payload.get("directories").is_none());

        let invalid_feed = invalid_feed_for_rollback();
        assert!(
            persistence
                .save_state(DurableStateWrite {
                    sessions: &sessions.export_state(),
                    workspaces: Some(&workspaces.export_state()),
                    settings: Some(&DurableSessionSettings::default()),
                    workspace_configs: Some(&BTreeMap::new()),
                    providers: Some(&DurableProviderState::default()),
                    usage: Some(&UsageLedger::default()),
                    idempotency: Some(&BTreeMap::new()),
                    run_summaries: Some(&run_summaries),
                    run_runtime_configs: None,
                    run_context_checkpoints: None,
                    run_plans: Some(&BTreeMap::new()),
                    run_messages: None,
                    run_activities: None,
                    filesystem_records: Some(&[DurableFilesystemRecord {
                        checkpoints: Vec::new(),
                        ..filesystem_records[0].clone()
                    }]),
                    feed: Some(&invalid_feed),
                })
                .is_err()
        );
        assert_eq!(
            persistence.load_run_plan(run_id).unwrap(),
            run_plans[&run_id]
        );
        assert_eq!(
            persistence.load_run_summary(run_id).unwrap(),
            run_summaries.get(&run_id).cloned()
        );
        assert_eq!(
            persistence
                .load_session_settings()
                .unwrap()
                .approval_policies,
            settings.approval_policies
        );
        assert_eq!(persistence.load_workspace_configs().unwrap(), configs);
        assert_eq!(
            persistence.load_provider_health().unwrap(),
            provider_state.health
        );
        assert_eq!(persistence.load_provider_usage().unwrap(), usage);
        assert_eq!(persistence.load_idempotency_records().unwrap(), idempotency);
        assert_eq!(persistence.load_run_summaries().unwrap(), run_summaries);
        let retained_filesystem = persistence
            .load_filesystem_record(session.id)
            .unwrap()
            .unwrap();
        let mut expected_filesystem_payload = filesystem_records[0].payload.clone();
        expected_filesystem_payload["filesystem"]["next_sequence"] = serde_json::json!(2);
        assert_eq!(retained_filesystem.payload, expected_filesystem_payload);
        assert_eq!(retained_filesystem.checkpoints, vec![checkpoint.clone()]);

        let connection = Connection::open(&path).unwrap();
        let (stored_policy, policy_storage_type): (String, String) = connection
            .query_row(
                "SELECT policy_write, typeof(policy_write) FROM session_settings
                 WHERE session_id=?1",
                [session.id.as_uuid().as_bytes().as_slice()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(stored_policy, "require_approval");
        assert_eq!(policy_storage_type, "text");
        let settings_columns = connection
            .prepare("PRAGMA table_info(session_settings)")
            .unwrap()
            .query_map([], |row| row.get::<_, String>(1))
            .unwrap()
            .collect::<std::result::Result<BTreeSet<_>, _>>()
            .unwrap();
        assert!(!settings_columns.contains("approval_policy"));
        let repo_plan: String = connection
            .query_row(
                "EXPLAIN QUERY PLAN SELECT path FROM session_repositories
                 WHERE session_id=?1 AND path=?2",
                params![
                    session.id.as_uuid().as_bytes().as_slice(),
                    "repositories/example"
                ],
                |row| row.get(3),
            )
            .unwrap();
        assert!(
            repo_plan.contains("session_repositories_by_path"),
            "{repo_plan}"
        );
        let directory_plan: String = connection
            .query_row(
                "EXPLAIN QUERY PLAN SELECT source FROM session_directories
                 WHERE session_id=?1 AND path=?2",
                params![session.id.as_uuid().as_bytes().as_slice(), "docs"],
                |row| row.get(3),
            )
            .unwrap();
        assert!(
            directory_plan.contains("sqlite_autoindex_session_directories_2"),
            "{directory_plan}"
        );
        let summary_text: Option<String> = connection
            .query_row(
                "SELECT summary FROM run_summaries WHERE run_id=?1",
                [run_id.as_uuid().as_bytes().as_slice()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(summary_text.as_deref(), Some("finished"));
        let plan_rows: i64 = connection
            .query_row(
                "SELECT count(*) FROM run_plan_steps WHERE run_id=?1",
                [run_id.as_uuid().as_bytes().as_slice()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(plan_rows, run_plans[&run_id].steps.len() as i64);
        let plan: String = connection
            .query_row(
                "EXPLAIN QUERY PLAN SELECT revision, config FROM workspace_configs WHERE workspace_id=?1",
                [workspace.id.as_uuid().as_bytes().as_slice()],
                |row| row.get(3),
            )
            .unwrap();
        assert!(plan.contains("PRIMARY KEY"), "{plan}");
        let usage_plan: String = connection
            .query_row(
                "EXPLAIN QUERY PLAN SELECT SUM(input_tokens) FROM run_summaries WHERE session_id=?1",
                [session.id.as_uuid().as_bytes().as_slice()],
                |row| row.get(3),
            )
            .unwrap();
        assert!(
            usage_plan.contains("runs_by_session_activity"),
            "{usage_plan}"
        );
        let idempotency_plan: String = connection
            .query_row(
                "EXPLAIN QUERY PLAN SELECT request_id FROM idempotency_records
                 WHERE expires_at IS NOT NULL AND expires_at<=?1
                 ORDER BY expires_at, request_id LIMIT 32",
                [Timestamp::now().as_unix_millis() as i64],
                |row| row.get(3),
            )
            .unwrap();
        assert!(
            idempotency_plan.contains("idempotency_expiry"),
            "{idempotency_plan}"
        );
        let run_plan: String = connection
            .query_row(
                "EXPLAIN QUERY PLAN SELECT run_id FROM run_summaries
                 WHERE session_id=?1 ORDER BY updated_at DESC, run_id DESC LIMIT 50",
                [session.id.as_uuid().as_bytes().as_slice()],
                |row| row.get(3),
            )
            .unwrap();
        assert!(run_plan.contains("runs_by_session_activity"), "{run_plan}");
        let filesystem_plan: String = connection
            .query_row(
                "EXPLAIN QUERY PLAN SELECT payload FROM session_filesystems WHERE session_id=?1",
                [session.id.as_uuid().as_bytes().as_slice()],
                |row| row.get(3),
            )
            .unwrap();
        assert!(filesystem_plan.contains("PRIMARY KEY"), "{filesystem_plan}");
        let filesystem_change_plan: String = connection
            .query_row(
                "EXPLAIN QUERY PLAN SELECT sequence FROM filesystem_changes
                 WHERE session_id=?1 AND sequence>?2 ORDER BY sequence LIMIT 32",
                params![session.id.as_uuid().as_bytes().as_slice(), 0_i64],
                |row| row.get(3),
            )
            .unwrap();
        assert!(
            filesystem_change_plan.contains("PRIMARY KEY"),
            "{filesystem_change_plan}"
        );
        let checkpoint_file_plan: String = connection
            .query_row(
                "EXPLAIN QUERY PLAN SELECT content_hash FROM checkpoint_files
                 WHERE session_id=?1 AND checkpoint_id=?2 ORDER BY path",
                params![
                    session.id.as_uuid().as_bytes().as_slice(),
                    checkpoint_id.as_uuid().as_bytes().as_slice()
                ],
                |row| row.get(3),
            )
            .unwrap();
        assert!(
            checkpoint_file_plan.contains("PRIMARY KEY"),
            "{checkpoint_file_plan}"
        );
        let run_message_plan: String = connection
            .query_row(
                "EXPLAIN QUERY PLAN SELECT role, content_hash FROM run_messages
                 WHERE run_id=?1 ORDER BY ordinal",
                [run_id.as_uuid().as_bytes().as_slice()],
                |row| row.get(3),
            )
            .unwrap();
        assert!(
            run_message_plan.contains("PRIMARY KEY"),
            "{run_message_plan}"
        );
        let activity_plan: String = connection
            .query_row(
                "EXPLAIN QUERY PLAN SELECT activity_id FROM run_activities
                 WHERE session_id=?1 ORDER BY started_at DESC, activity_id DESC LIMIT 20",
                [session.id.as_uuid().as_bytes().as_slice()],
                |row| row.get(3),
            )
            .unwrap();
        assert!(
            activity_plan.contains("run_activities_by_session_time"),
            "{activity_plan}"
        );
        let (content_codec, content_count): (i64, i64) = connection
            .query_row(
                "SELECT MAX(codec), COUNT(*) FROM content_blobs",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(content_codec, 1);
        assert!(content_count > 0 && content_count < 9);
        let (filesystem_codec, raw_size, payload_size): (i64, i64, i64) = connection
            .query_row(
                "SELECT payload_codec, raw_size, length(payload) FROM session_filesystems
                 WHERE session_id=?1",
                [session.id.as_uuid().as_bytes().as_slice()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(filesystem_codec, 1);
        assert!(payload_size < raw_size);
        let config_plan: String = connection
            .query_row(
                "EXPLAIN QUERY PLAN SELECT config FROM provider_configs WHERE provider_id=?1",
                [provider_id.as_str()],
                |row| row.get(3),
            )
            .unwrap();
        assert!(config_plan.contains("PRIMARY KEY"), "{config_plan}");
        let usage_plan: String = connection
            .query_row(
                "EXPLAIN QUERY PLAN SELECT requests FROM provider_usage_totals
                 WHERE provider_id=?1 AND model_id=?2",
                params![provider_id.as_str(), model_id.as_str()],
                |row| row.get(3),
            )
            .unwrap();
        assert!(usage_plan.contains("PRIMARY KEY"), "{usage_plan}");
        drop(connection);

        let empty_filesystem_records = [DurableFilesystemRecord {
            checkpoints: Vec::new(),
            ..filesystem_records[0].clone()
        }];
        persistence
            .save_state(DurableStateWrite {
                sessions: &sessions.export_state(),
                workspaces: Some(&workspaces.export_state()),
                settings: Some(&settings),
                workspace_configs: Some(&configs),
                providers: Some(&provider_state),
                usage: Some(&usage),
                idempotency: Some(&idempotency),
                run_summaries: Some(&run_summaries),
                run_runtime_configs: None,
                run_context_checkpoints: None,
                run_plans: None,
                run_messages: None,
                run_activities: None,
                filesystem_records: Some(&empty_filesystem_records),
                feed: None,
            })
            .unwrap();
        assert!(
            persistence
                .load_filesystem_record(session.id)
                .unwrap()
                .unwrap()
                .checkpoints
                .is_empty()
        );
        let connection = Connection::open(&path).unwrap();
        let content_count: i64 = connection
            .query_row("SELECT COUNT(*) FROM content_objects", [], |row| row.get(0))
            .unwrap();
        // Message tool calls and tool attempts are inline JSON now, so the
        // reachable content objects are the transcript, context checkpoint,
        // logical tool-call arguments, filesystem undo, and run instructions.
        assert!(
            content_count >= 7,
            "retained transcript, context checkpoint, logical tool-call, filesystem undo, and run-instruction content remain reachable"
        );
        drop(connection);
        assert_eq!(
            persistence
                .prune_expired_idempotency_records(Timestamp::now())
                .unwrap(),
            1
        );
        assert!(persistence.load_idempotency_records().unwrap().is_empty());
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn streamed_messages_are_append_only_and_paged_by_keyset() {
        let path = std::env::temp_dir().join(format!("loom-persistence-{}.db", Uuid::new_v4()));
        let persistence = FilePersistence::open(&path).unwrap();
        let mut manager = SessionManager::default();
        let (session, _) = manager
            .create_in_workspace(WorkspaceId::new(), "Streamed messages")
            .unwrap();
        let run_id = RunId::new();
        let summary = DurableRunSummary {
            snapshot: AgentRunSnapshot {
                id: run_id,
                attempt_id: loom_core::RunAttemptId::new(),
                control_revision: 0,
                session_id: session.id,
                task: "stream fragments".to_owned(),
                model: ModelId::new("deterministic-model"),
                state: AgentRunState::Executing,
                started_at: Timestamp::from_unix_millis(1000),
                updated_at: Timestamp::from_unix_millis(1000),
                completed_at: None,
                summary: None,
                evidence: Vec::new(),
            },
            usage: UsageSnapshot::default(),
            attempts: None,
            execution_state: None,
            interactions: None,
        };
        let run_summaries = BTreeMap::from([(run_id, summary)]);
        let large_content = "0123456789".repeat(60_000);
        let run_messages = BTreeMap::from([(
            run_id,
            vec![
                DurableRunMessage {
                    timeline_ordinal: 0,
                    role: loom_model::MessageRole::User,
                    content: "question".to_owned(),
                    name: None,
                    tool_call_id: None,
                    tool_calls: Vec::new(),
                },
                DurableRunMessage {
                    timeline_ordinal: 0,
                    role: loom_model::MessageRole::Assistant,
                    content: "seed".to_owned(),
                    name: None,
                    tool_call_id: None,
                    tool_calls: Vec::new(),
                },
                DurableRunMessage {
                    timeline_ordinal: 0,
                    role: loom_model::MessageRole::Assistant,
                    content: String::new(),
                    name: None,
                    tool_call_id: None,
                    tool_calls: Vec::new(),
                },
                DurableRunMessage {
                    timeline_ordinal: 0,
                    role: loom_model::MessageRole::User,
                    content: large_content.clone(),
                    name: None,
                    tool_call_id: None,
                    tool_calls: Vec::new(),
                },
            ],
        )]);
        let oversized_fragment = vec![b'x'; MAX_MESSAGE_FRAGMENT_BYTES + 1];
        assert_eq!(
            persistence
                .append_run_message_fragment(run_id, session.id, 1, 0, 4, &oversized_fragment,)
                .unwrap_err()
                .code,
            ErrorCode::InvalidRequest
        );
        persistence
            .save_state(DurableStateWrite {
                sessions: &manager.export_state(),
                workspaces: None,
                settings: None,
                workspace_configs: None,
                providers: None,
                usage: None,
                idempotency: None,
                run_summaries: Some(&run_summaries),
                run_runtime_configs: None,
                run_context_checkpoints: None,
                run_plans: None,
                run_messages: Some(&run_messages),
                run_activities: None,
                filesystem_records: None,
                feed: None,
            })
            .unwrap();

        for (message_ordinal, fragment_ordinal, byte_offset) in [
            (u64::MAX, 0, 0),
            (1, u64::MAX, 4),
            (1, 0, u64::MAX),
            (1, 0, i64::MAX as u64),
        ] {
            assert_eq!(
                persistence
                    .append_run_message_fragment(
                        run_id,
                        session.id,
                        message_ordinal,
                        fragment_ordinal,
                        byte_offset,
                        b"x",
                    )
                    .unwrap_err()
                    .code,
                ErrorCode::InvalidRequest
            );
        }

        assert_eq!(
            persistence
                .append_run_message_fragment(run_id, AgentSessionId::new(), 1, 0, 4, b"x")
                .unwrap_err()
                .code,
            ErrorCode::WorkspaceAccessDenied
        );
        assert_eq!(
            persistence
                .append_run_message_fragment(run_id, session.id, 1, 0, 4, b"")
                .unwrap_err()
                .code,
            ErrorCode::InvalidRequest
        );
        assert_eq!(
            persistence
                .append_run_message_fragment(run_id, session.id, 1, 0, 4, &[0xff])
                .unwrap_err()
                .code,
            ErrorCode::InvalidRequest
        );
        assert_eq!(
            persistence
                .append_run_message_fragment(run_id, session.id, 0, 0, 0, b"not assistant")
                .unwrap_err()
                .code,
            ErrorCode::InvalidRequest
        );
        persistence
            .append_run_message_fragment(run_id, session.id, 1, 0, 4, b"hello ")
            .unwrap();
        persistence
            .append_run_message_fragment(run_id, session.id, 1, 0, 4, b"hello ")
            .unwrap();
        persistence
            .append_run_message_fragment(run_id, session.id, 1, 1, 10, b"world")
            .unwrap();
        assert_eq!(
            persistence
                .append_run_message_fragment(run_id, session.id, 1, 0, 4, b"conflict")
                .unwrap_err()
                .code,
            ErrorCode::InvalidRequest
        );
        assert_eq!(
            persistence
                .load_run_message_content_range(run_id, 999, 0, 1)
                .unwrap_err()
                .code,
            ErrorCode::InvalidRequest
        );
        assert_eq!(
            persistence
                .load_run_message_content_range(
                    run_id,
                    1,
                    0,
                    MAX_CONTENT_RANGE_BYTES.saturating_add(1),
                )
                .unwrap_err()
                .code,
            ErrorCode::InvalidRequest
        );
        let newest_page = persistence
            .load_run_message_page(run_id, Some(2), 1)
            .unwrap();
        assert_eq!(newest_page.len(), 1);
        assert_eq!(newest_page[0].ordinal, 1);
        assert_eq!(newest_page[0].content_bytes, 15);
        assert_eq!(
            persistence
                .load_run_message_content_range(run_id, 1, u64::MAX, 1)
                .unwrap_err()
                .code,
            ErrorCode::InvalidRequest
        );
        assert_eq!(
            persistence
                .load_run_message_content_range(run_id, 1, 7, 5)
                .unwrap(),
            b"lo wo"
        );
        assert_eq!(
            persistence
                .load_run_message_content_range(run_id, 3, (CONTENT_PART_BYTES - 3) as u64, 10,)
                .unwrap(),
            b"1234567890"
        );
        assert!(
            persistence
                .load_run_message_content_range(run_id, 3, 0, 0)
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            persistence.load_run_messages(run_id).unwrap()[1].content,
            "seedhello world"
        );
        assert_eq!(
            persistence
                .load_run_message_page(run_id, Some(1), 1)
                .unwrap()[0]
                .ordinal,
            0
        );
        assert!(
            persistence
                .append_run_message_fragment(run_id, session.id, 1, 2, 12, b"!")
                .is_err()
        );
        assert_eq!(
            persistence
                .next_run_message_fragment_position(run_id, 1)
                .unwrap(),
            (2, 15)
        );
        persistence
            .append_run_message_fragment(run_id, session.id, 1, 2, 15, b"!")
            .unwrap();
        persistence
            .append_run_message_fragment(run_id, session.id, 2, 0, 0, b"part")
            .unwrap();
        persistence
            .append_run_message_fragment(run_id, session.id, 2, 1, 4, b"ial")
            .unwrap();
        drop(persistence);
        let persistence = FilePersistence::open(&path).unwrap();
        let newest_page = persistence
            .load_run_message_page(run_id, Some(3), 1)
            .unwrap();
        assert_eq!(newest_page[0].ordinal, 2);
        assert_eq!(newest_page[0].content_bytes, 7);
        assert_eq!(
            persistence
                .load_run_message_content_range(run_id, 2, 2, 4)
                .unwrap(),
            b"rtia"
        );
        assert_eq!(
            persistence.load_run_messages(run_id).unwrap()[2].content,
            "partial"
        );
        assert_eq!(
            persistence.load_run_messages(run_id).unwrap()[3].content,
            large_content
        );
        assert_eq!(
            persistence
                .load_run_message_page(run_id, Some(2), 1)
                .unwrap()[0]
                .ordinal,
            1
        );
        assert_eq!(
            persistence
                .load_run_message_page(run_id, Some(1), 1)
                .unwrap()[0]
                .ordinal,
            0
        );
        assert_eq!(
            persistence.load_run_messages(run_id).unwrap()[1].content,
            "seedhello world!"
        );
        persistence
            .save_state(DurableStateWrite {
                sessions: &manager.export_state(),
                workspaces: None,
                settings: None,
                workspace_configs: None,
                providers: None,
                usage: None,
                idempotency: None,
                run_summaries: Some(&run_summaries),
                run_runtime_configs: None,
                run_context_checkpoints: None,
                run_plans: None,
                run_messages: Some(&run_messages),
                run_activities: None,
                filesystem_records: None,
                feed: None,
            })
            .unwrap();
        let after_stale_snapshot = persistence.load_run_messages(run_id).unwrap();
        assert_eq!(after_stale_snapshot[1].content, "seedhello world!");
        assert_eq!(after_stale_snapshot[2].content, "partial");

        let mut mismatched_messages = run_messages.clone();
        mismatched_messages.get_mut(&run_id).unwrap()[1].content = "replacement base".to_owned();
        mismatched_messages.get_mut(&run_id).unwrap()[2].content = "replacement tail".to_owned();
        persistence
            .save_state(DurableStateWrite {
                sessions: &manager.export_state(),
                workspaces: None,
                settings: None,
                workspace_configs: None,
                providers: None,
                usage: None,
                idempotency: None,
                run_summaries: Some(&run_summaries),
                run_runtime_configs: None,
                run_context_checkpoints: None,
                run_plans: None,
                run_messages: Some(&mismatched_messages),
                run_activities: None,
                filesystem_records: None,
                feed: None,
            })
            .unwrap();
        let after_mismatch = persistence.load_run_messages(run_id).unwrap();
        assert_eq!(after_mismatch[1].content, "seedhello world!");
        assert_eq!(after_mismatch[2].content, "partial");
        assert_eq!(stored_fragment_count(&path, run_id), 5);

        let mut assembled_messages = run_messages.clone();
        assembled_messages.get_mut(&run_id).unwrap()[1].content = "seedhello world!".to_owned();
        assembled_messages.get_mut(&run_id).unwrap()[2].content = "partial".to_owned();
        let invalid_feed = invalid_feed_for_rollback();
        assert!(
            persistence
                .save_state(DurableStateWrite {
                    sessions: &manager.export_state(),
                    workspaces: None,
                    settings: None,
                    workspace_configs: None,
                    providers: None,
                    usage: None,
                    idempotency: None,
                    run_summaries: Some(&run_summaries),
                    run_runtime_configs: None,
                    run_context_checkpoints: None,
                    run_plans: None,
                    run_messages: Some(&assembled_messages),
                    run_activities: None,
                    filesystem_records: None,
                    feed: Some(&invalid_feed),
                })
                .is_err()
        );
        let after_rollback = persistence.load_run_messages(run_id).unwrap();
        assert_eq!(after_rollback[1].content, "seedhello world!");
        assert_eq!(after_rollback[2].content, "partial");
        assert_eq!(stored_fragment_count(&path, run_id), 5);
        persistence
            .save_state(DurableStateWrite {
                sessions: &manager.export_state(),
                workspaces: None,
                settings: None,
                workspace_configs: None,
                providers: None,
                usage: None,
                idempotency: None,
                run_summaries: Some(&run_summaries),
                run_runtime_configs: None,
                run_context_checkpoints: None,
                run_plans: None,
                run_messages: Some(&assembled_messages),
                run_activities: None,
                filesystem_records: None,
                feed: None,
            })
            .unwrap();
        let after_consolidation = persistence.load_run_messages(run_id).unwrap();
        assert_eq!(after_consolidation[1].content, "seedhello world!");
        assert_eq!(after_consolidation[2].content, "partial");
        assert_eq!(stored_fragment_count(&path, run_id), 0);
        assert!(
            persistence
                .load_run_message_page(run_id, None, 101)
                .is_err()
        );
        assert!(
            persistence
                .load_run_message_content_range(
                    run_id,
                    1,
                    0,
                    MAX_CONTENT_RANGE_BYTES.saturating_add(1)
                )
                .is_err()
        );
        let connection = Connection::open(&path).unwrap();
        let page_plan: String = connection
            .query_row(
                "EXPLAIN QUERY PLAN SELECT ordinal FROM run_messages
                 WHERE run_id=?1 AND ordinal<?2 ORDER BY ordinal DESC LIMIT 20",
                params![run_id.as_uuid().as_bytes().as_slice(), 2_i64],
                |row| row.get(3),
            )
            .unwrap();
        assert!(page_plan.contains("PRIMARY KEY"), "{page_plan}");
        let content_range_plan: String = connection
            .query_row(
                "EXPLAIN QUERY PLAN SELECT byte_offset, byte_length, blob_hash
                 FROM content_parts
                 WHERE content_hash=?1 AND byte_offset<?2
                   AND byte_offset + byte_length > ?3
                 ORDER BY byte_offset",
                params![vec![0_u8; 32], CONTENT_PART_BYTES as i64, 0_i64],
                |row| row.get(3),
            )
            .unwrap();
        assert!(
            content_range_plan.contains("content_hash=? AND byte_offset<?"),
            "{content_range_plan}"
        );
        let large_content_hash: Vec<u8> = connection
            .query_row(
                "SELECT content_hash FROM run_messages WHERE run_id=?1 AND ordinal=3",
                [run_id.as_uuid().as_bytes().as_slice()],
                |row| row.get(0),
            )
            .unwrap();
        connection
            .execute(
                "DELETE FROM content_parts WHERE content_hash=?1 AND ordinal=1",
                [large_content_hash],
            )
            .unwrap();
        drop(connection);
        assert_eq!(
            persistence
                .load_run_message_content_range(run_id, 3, CONTENT_PART_BYTES as u64 + 1, 8)
                .unwrap_err()
                .code,
            ErrorCode::MalformedPayload
        );
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn run_message_pages_reject_invalid_limits_and_cursors() {
        let path = std::env::temp_dir().join(format!("loom-persistence-{}.db", Uuid::new_v4()));
        let store = FilePersistence::open(&path).unwrap();
        store
            .save_state_with_sessions(&SessionManager::default().export_state())
            .unwrap();
        let run_id = RunId::new();
        assert!(
            store
                .load_run_message_page(run_id, None, 1)
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            store
                .load_run_message_page(run_id, None, 0)
                .unwrap_err()
                .code,
            ErrorCode::InvalidRequest
        );
        assert_eq!(
            store
                .load_run_message_page(run_id, None, MAX_RUN_MESSAGE_PAGE_SIZE + 1)
                .unwrap_err()
                .code,
            ErrorCode::InvalidRequest
        );
        assert_eq!(
            store
                .load_run_message_page(run_id, Some(u64::MAX), 1)
                .unwrap_err()
                .code,
            ErrorCode::InvalidRequest
        );
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn run_interactions_round_trip_and_commit_atomically_with_the_feed() {
        let path = std::env::temp_dir().join(format!("loom-persistence-{}.db", Uuid::new_v4()));
        let persistence = FilePersistence::open(&path).unwrap();
        let mut manager = SessionManager::default();
        let (session, _) = manager
            .create_in_workspace(WorkspaceId::new(), "Run interactions")
            .unwrap();
        let run_id = RunId::new();
        let attempt_id = RunAttemptId::new();
        let interaction_id = InteractionId::new();
        let prompt = "Which branch should I use?".to_owned();
        let created_at = Timestamp::from_unix_millis(1_000);
        let attempt = AgentRunAttemptRecord {
            run_id,
            session_id: session.id,
            id: attempt_id,
            number: 1,
            state: AgentRunState::NeedsInput,
            checkpoint_id: None,
            started_at: created_at,
            completed_at: None,
        };
        let mut execution = AgentExecutionStateRecord {
            run_id,
            session_id: session.id,
            attempt_id,
            control_revision: 1,
            state: AgentRunState::NeedsInput,
            step_id: None,
            step_index: 0,
            provider_cursor: 0,
            next_message_id: 2,
            active_message_id: None,
            last_project_message_sequence: 7,
            pending_tool_execution: None,
            pending_project_join: None,
            pending_approval: None,
            pending_input: Some(prompt.clone()),
            last_failed_call: None,
        };
        let mut interaction = AgentInteractionRecord {
            id: interaction_id,
            run_id,
            session_id: session.id,
            attempt_id,
            control_revision: 1,
            kind: AgentInteractionKind::UserInput,
            status: AgentInteractionStatus::Pending,
            tool_call_id: None,
            prompt: prompt.clone(),
            decision: None,
            created_at,
            resolved_at: None,
        };
        let mut summary = DurableRunSummary {
            snapshot: AgentRunSnapshot {
                id: run_id,
                attempt_id,
                control_revision: 1,
                session_id: session.id,
                task: "choose a branch".to_owned(),
                model: ModelId::new("deterministic-model"),
                state: AgentRunState::NeedsInput,
                started_at: created_at,
                updated_at: created_at,
                completed_at: None,
                summary: None,
                evidence: Vec::new(),
            },
            usage: UsageSnapshot::default(),
            attempts: Some(vec![attempt.clone()]),
            execution_state: Some(execution.clone()),
            interactions: Some(vec![interaction.clone()]),
        };
        let run_summaries = BTreeMap::from([(run_id, summary.clone())]);
        let input_event = loom_protocol::ServerEventEnvelope {
            protocol_version: loom_protocol::CURRENT_PROTOCOL_VERSION,
            sequence: EventSequence::new(1),
            session_id: session.id,
            event: loom_protocol::ServerEvent::Agent {
                event: loom_protocol::AgentEvent::NeedsInput {
                    run_id,
                    attempt_id,
                    control_revision: 1,
                    interaction_id,
                    prompt: prompt.clone(),
                },
            },
        };
        let mut feed = DurableFeedState {
            next_sequence: EventSequence::new(1),
            retention_limit: 16,
            events: vec![input_event.clone()],
            workspace_events: Vec::new(),
        };
        let initial_sessions = manager.export_state();
        let save = |sessions: &SessionManagerState,
                    summary: &BTreeMap<RunId, DurableRunSummary>,
                    feed: &DurableFeedState|
         -> Result<()> {
            persistence.save_state(DurableStateWrite {
                sessions,
                workspaces: None,
                settings: None,
                workspace_configs: None,
                providers: None,
                usage: None,
                idempotency: None,
                run_summaries: Some(summary),
                run_runtime_configs: None,
                run_context_checkpoints: None,
                run_plans: None,
                run_messages: None,
                run_activities: None,
                filesystem_records: None,
                feed: Some(feed),
            })
        };
        save(&initial_sessions, &run_summaries, &feed).unwrap();
        assert_eq!(
            persistence.load_run_attempts(run_id).unwrap(),
            vec![attempt.clone()]
        );
        assert_eq!(
            persistence.load_run_execution_state(run_id).unwrap(),
            Some(execution.clone())
        );
        assert_eq!(
            persistence.load_run_interactions(run_id).unwrap(),
            vec![interaction.clone()]
        );

        interaction.status = AgentInteractionStatus::Answered;
        interaction.resolved_at = Some(Timestamp::from_unix_millis(2_000));
        summary.snapshot.state = AgentRunState::Executing;
        summary.snapshot.control_revision = 2;
        summary.snapshot.updated_at = Timestamp::from_unix_millis(2_000);
        summary.attempts = Some(vec![AgentRunAttemptRecord {
            state: AgentRunState::Executing,
            ..attempt.clone()
        }]);
        execution.control_revision = 2;
        execution.state = AgentRunState::Executing;
        execution.next_message_id = 3;
        execution.pending_input = None;
        execution.pending_tool_execution = Some(loom_model::ToolCall {
            id: loom_core::ToolCallId::new(),
            name: "write_file".to_owned(),
            arguments: serde_json::json!({"path": "src/main.rs", "content": "new"}),
        });
        summary.execution_state = Some(execution.clone());
        summary.interactions = Some(vec![interaction.clone()]);
        let resolved_event = loom_protocol::ServerEventEnvelope {
            protocol_version: loom_protocol::CURRENT_PROTOCOL_VERSION,
            sequence: EventSequence::new(2),
            session_id: session.id,
            event: loom_protocol::ServerEvent::Agent {
                event: loom_protocol::AgentEvent::UserMessage {
                    run_id,
                    attempt_id,
                    control_revision: 2,
                    interaction_id: Some(interaction_id),
                    text: "Use the release branch.".to_owned(),
                },
            },
        };
        feed.events.push(resolved_event);
        feed.next_sequence = EventSequence::new(0);
        let resolved_summaries = BTreeMap::from([(run_id, summary.clone())]);
        let mut resolving_sessions = initial_sessions.clone();
        resolving_sessions
            .sessions
            .get_mut(&session.id)
            .unwrap()
            .state = loom_core::AgentSessionState::Executing;
        assert_eq!(
            save(&resolving_sessions, &resolved_summaries, &feed)
                .unwrap_err()
                .code,
            ErrorCode::MalformedPayload
        );
        assert_eq!(
            persistence.load_run_interactions(run_id).unwrap(),
            vec![AgentInteractionRecord {
                status: AgentInteractionStatus::Pending,
                resolved_at: None,
                ..interaction.clone()
            }]
        );
        assert_eq!(
            persistence.load_run_attempts(run_id).unwrap(),
            vec![attempt.clone()]
        );
        assert_eq!(
            persistence.load_run_execution_state(run_id).unwrap(),
            Some(AgentExecutionStateRecord {
                control_revision: 1,
                state: AgentRunState::NeedsInput,
                next_message_id: 2,
                pending_input: Some(prompt.clone()),
                pending_tool_execution: None,
                ..execution.clone()
            })
        );
        assert_eq!(
            persistence
                .load_run_summary(run_id)
                .unwrap()
                .unwrap()
                .snapshot
                .state,
            AgentRunState::NeedsInput
        );
        assert_eq!(
            persistence.load_sessions().unwrap().unwrap(),
            initial_sessions
        );
        let persisted_feed = persistence.load_feed_state().unwrap().unwrap();
        assert_eq!(persisted_feed.next_sequence, EventSequence::new(1));
        assert_eq!(persisted_feed.events, vec![input_event.clone()]);

        drop(persistence);
        let reopened = FilePersistence::open(&path).unwrap();
        assert_eq!(
            reopened
                .load_run_summary(run_id)
                .unwrap()
                .unwrap()
                .snapshot
                .state,
            AgentRunState::NeedsInput
        );
        assert_eq!(
            reopened.load_run_execution_state(run_id).unwrap(),
            Some(AgentExecutionStateRecord {
                control_revision: 1,
                state: AgentRunState::NeedsInput,
                next_message_id: 2,
                pending_input: Some(prompt),
                pending_tool_execution: None,
                ..execution.clone()
            })
        );
        assert_eq!(
            reopened.load_run_interactions(run_id).unwrap()[0].status,
            AgentInteractionStatus::Pending
        );
        assert_eq!(reopened.load_sessions().unwrap().unwrap(), initial_sessions);
        let reopened_feed = reopened.load_feed_state().unwrap().unwrap();
        assert_eq!(reopened_feed.next_sequence, EventSequence::new(1));
        assert_eq!(reopened_feed.events, vec![input_event]);

        feed.next_sequence = EventSequence::new(2);
        reopened
            .save_state(DurableStateWrite {
                sessions: &resolving_sessions,
                workspaces: None,
                settings: None,
                workspace_configs: None,
                providers: None,
                usage: None,
                idempotency: None,
                run_summaries: Some(&resolved_summaries),
                run_runtime_configs: None,
                run_context_checkpoints: None,
                run_plans: None,
                run_messages: None,
                run_activities: None,
                filesystem_records: None,
                feed: Some(&feed),
            })
            .unwrap();
        assert_eq!(
            reopened
                .load_run_summary(run_id)
                .unwrap()
                .unwrap()
                .snapshot
                .state,
            AgentRunState::Executing
        );
        assert_eq!(
            reopened.load_run_execution_state(run_id).unwrap(),
            Some(execution.clone())
        );
        assert_eq!(
            reopened.load_run_interactions(run_id).unwrap()[0].status,
            AgentInteractionStatus::Answered
        );
        assert_eq!(
            reopened.load_feed_state().unwrap().unwrap().next_sequence,
            EventSequence::new(2)
        );
        summary.snapshot.state = AgentRunState::Evaluating;
        summary.snapshot.updated_at = Timestamp::from_unix_millis(3_000);
        summary.attempts = Some(vec![AgentRunAttemptRecord {
            state: AgentRunState::Evaluating,
            ..attempt
        }]);
        execution.state = AgentRunState::Evaluating;
        summary.execution_state = Some(execution.clone());
        let evaluating_summaries = BTreeMap::from([(run_id, summary)]);
        reopened
            .save_state(DurableStateWrite {
                sessions: &resolving_sessions,
                workspaces: None,
                settings: None,
                workspace_configs: None,
                providers: None,
                usage: None,
                idempotency: None,
                run_summaries: Some(&evaluating_summaries),
                run_runtime_configs: None,
                run_context_checkpoints: None,
                run_plans: None,
                run_messages: None,
                run_activities: None,
                filesystem_records: None,
                feed: Some(&feed),
            })
            .unwrap();
        assert_eq!(
            reopened.load_run_execution_state(run_id).unwrap(),
            Some(execution)
        );
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn tool_attempt_state_storage_tracks_activity_outcomes_and_intents() {
        let path = std::env::temp_dir().join(format!("loom-persistence-{}.db", Uuid::new_v4()));
        let persistence = FilePersistence::open(&path).unwrap();
        let mut sessions = SessionManager::default();
        let (session, _) = sessions
            .create_in_workspace(WorkspaceId::new(), "Tool attempt owner")
            .unwrap();
        let run_id = RunId::new();
        let attempt_id = RunAttemptId::new();
        let started_at = Timestamp::from_unix_millis(1000);
        let queued_call = loom_model::ToolCall {
            id: loom_core::ToolCallId::new(),
            name: "read_file".to_owned(),
            arguments: serde_json::json!({"path": "queued"}),
        };
        let unknown_call = loom_model::ToolCall {
            id: loom_core::ToolCallId::new(),
            name: "write_file".to_owned(),
            arguments: serde_json::json!({"path": "unknown"}),
        };
        let statuses = [
            (queued_call.clone(), AgentActivityStatus::Started),
            (unknown_call.clone(), AgentActivityStatus::Started),
            (
                loom_model::ToolCall {
                    id: loom_core::ToolCallId::new(),
                    name: "inspect".to_owned(),
                    arguments: serde_json::json!({}),
                },
                AgentActivityStatus::Started,
            ),
            (
                loom_model::ToolCall {
                    id: loom_core::ToolCallId::new(),
                    name: "failed".to_owned(),
                    arguments: serde_json::json!({}),
                },
                AgentActivityStatus::Failed,
            ),
            (
                loom_model::ToolCall {
                    id: loom_core::ToolCallId::new(),
                    name: "ask_user".to_owned(),
                    arguments: serde_json::json!({}),
                },
                AgentActivityStatus::AwaitingInput,
            ),
            (
                loom_model::ToolCall {
                    id: loom_core::ToolCallId::new(),
                    name: "cancelled".to_owned(),
                    arguments: serde_json::json!({}),
                },
                AgentActivityStatus::Cancelled,
            ),
        ];
        let mut activities = statuses
            .into_iter()
            .enumerate()
            .map(|(ordinal, (call, status))| AgentActivityRecord {
                id: ActivityId::new(),
                run_id,
                timeline_ordinal: 0,
                parent_id: None,
                step_id: None,
                kind: AgentActivityKind::ToolCall,
                status,
                started_at: Timestamp::from_unix_millis(1000 + ordinal as u64),
                completed_at: None,
                elapsed_ms: None,
                data: AgentActivityData::ToolCall { call, result: None },
            })
            .collect::<Vec<_>>();

        let completed_call = loom_model::ToolCall {
            id: loom_core::ToolCallId::new(),
            name: "completed".to_owned(),
            arguments: serde_json::json!({}),
        };
        let completed_call_id = completed_call.id;
        activities.push(AgentActivityRecord {
            id: ActivityId::new(),
            run_id,
            timeline_ordinal: 0,
            parent_id: None,
            step_id: None,
            kind: AgentActivityKind::ToolCall,
            status: AgentActivityStatus::Completed,
            started_at: Timestamp::from_unix_millis(1100),
            completed_at: Some(Timestamp::from_unix_millis(1200)),
            elapsed_ms: Some(100),
            data: AgentActivityData::ToolCall {
                call: completed_call,
                result: Some(ToolResult {
                    tool_call_id: completed_call_id,
                    name: "completed".to_owned(),
                    success: true,
                    output: "done".to_owned(),
                }),
            },
        });

        let summary = DurableRunSummary {
            snapshot: AgentRunSnapshot {
                id: run_id,
                attempt_id,
                control_revision: 3,
                session_id: session.id,
                task: "Exercise tool-attempt states".to_owned(),
                model: ModelId::new("deterministic-model"),
                state: AgentRunState::Executing,
                started_at,
                updated_at: Timestamp::from_unix_millis(1200),
                completed_at: None,
                summary: None,
                evidence: Vec::new(),
            },
            usage: UsageSnapshot::default(),
            attempts: Some(vec![AgentRunAttemptRecord {
                run_id,
                session_id: session.id,
                id: attempt_id,
                number: 1,
                state: AgentRunState::Executing,
                checkpoint_id: None,
                started_at,
                completed_at: None,
            }]),
            execution_state: Some(AgentExecutionStateRecord {
                run_id,
                session_id: session.id,
                attempt_id,
                control_revision: 3,
                state: AgentRunState::Executing,
                step_id: None,
                step_index: 0,
                provider_cursor: 0,
                next_message_id: 0,
                active_message_id: None,
                last_project_message_sequence: 0,
                pending_tool_execution: Some(queued_call.clone()),
                pending_project_join: None,
                pending_approval: None,
                pending_input: None,
                last_failed_call: Some(unknown_call.clone()),
            }),
            interactions: None,
        };
        let summaries = BTreeMap::from([(run_id, summary)]);
        let activities = BTreeMap::from([(run_id, activities)]);
        persistence
            .save_state(DurableStateWrite {
                sessions: &sessions.export_state(),
                workspaces: None,
                settings: None,
                workspace_configs: None,
                providers: None,
                usage: None,
                idempotency: None,
                run_summaries: Some(&summaries),
                run_runtime_configs: None,
                run_context_checkpoints: None,
                run_plans: None,
                run_messages: None,
                run_activities: Some(&activities),
                filesystem_records: None,
                feed: None,
            })
            .unwrap();

        let attempts = persistence.load_run_tool_attempts(run_id).unwrap();
        assert_eq!(attempts.len(), 7);
        assert_eq!(
            attempts
                .iter()
                .find(|attempt| attempt.call_id == queued_call.id)
                .unwrap()
                .state,
            AgentToolAttemptState::Queued
        );
        assert_eq!(
            attempts
                .iter()
                .find(|attempt| attempt.call_id == unknown_call.id)
                .unwrap()
                .state,
            AgentToolAttemptState::OutcomeUnknown
        );
        assert_eq!(
            attempts
                .iter()
                .map(|attempt| attempt.state)
                .collect::<Vec<_>>(),
            vec![
                AgentToolAttemptState::Queued,
                AgentToolAttemptState::OutcomeUnknown,
                AgentToolAttemptState::Running,
                AgentToolAttemptState::Failed,
                AgentToolAttemptState::AwaitingInput,
                AgentToolAttemptState::Cancelled,
                AgentToolAttemptState::Completed,
            ]
        );
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn reconnect_feed_is_indexed_bounded_and_atomic_with_catalog_writes() {
        let path = std::env::temp_dir().join(format!("loom-persistence-{}.db", Uuid::new_v4()));
        let store = FilePersistence::open(&path).unwrap();
        let session_id = AgentSessionId::new();
        let workspace_id = WorkspaceId::new();
        let snapshot = AgentSessionSnapshot {
            id: session_id,
            workspace_id,
            name: "Feed test".to_owned(),
            state: AgentSessionState::Idle,
            created_at: Timestamp::from_unix_millis(10),
            updated_at: Timestamp::from_unix_millis(10),
        };
        let mut manager = SessionManager::default();
        manager
            .create_in_workspace_with_id(workspace_id, session_id, "Feed test")
            .unwrap();
        let second_session_id = AgentSessionId::new();
        manager
            .create_in_workspace_with_id(workspace_id, second_session_id, "Quiet feed")
            .unwrap();
        let second_snapshot = AgentSessionSnapshot {
            id: second_session_id,
            workspace_id,
            name: "Quiet feed".to_owned(),
            state: AgentSessionState::Idle,
            created_at: Timestamp::from_unix_millis(11),
            updated_at: Timestamp::from_unix_millis(11),
        };
        let sessions = manager.export_state();
        let feed = DurableFeedState {
            next_sequence: EventSequence::new(4),
            retention_limit: 2,
            events: [
                (1, session_id, snapshot.clone()),
                (2, second_session_id, second_snapshot),
                (3, session_id, snapshot.clone()),
                (4, session_id, snapshot.clone()),
            ]
            .into_iter()
            .map(
                |(sequence, session_id, event_snapshot)| ServerEventEnvelope {
                    protocol_version: loom_protocol::CURRENT_PROTOCOL_VERSION,
                    sequence: EventSequence::new(sequence),
                    session_id,
                    event: loom_protocol::ServerEvent::AgentSessionCreated {
                        snapshot: event_snapshot,
                    },
                },
            )
            .collect(),
            workspace_events: Vec::new(),
        };
        store
            .save_state_with_sessions_and_feed(&sessions, Some(&feed))
            .unwrap();
        let header = store.load_feed_header().unwrap().unwrap();
        assert_eq!(header.next_sequence, EventSequence::new(4));
        assert_eq!(header.retention_limit, 2);
        let first_cursor = store.load_feed_session_cursor(session_id).unwrap().unwrap();
        assert_eq!(first_cursor.first_sequence, EventSequence::new(1));
        assert_eq!(first_cursor.latest_sequence, EventSequence::new(4));
        assert_eq!(first_cursor.pruned_through, EventSequence::new(1));
        assert_eq!(
            first_cursor.oldest_retained_sequence,
            Some(EventSequence::new(3))
        );
        assert_eq!(
            store
                .load_feed_events_since(Some(session_id), Some(EventSequence::new(2)))
                .unwrap()
                .iter()
                .map(|event| event.sequence.value())
                .collect::<Vec<_>>(),
            vec![3, 4]
        );
        let loaded = store.load_feed_state().unwrap().unwrap();
        assert_eq!(loaded.next_sequence, EventSequence::new(4));
        assert_eq!(loaded.retention_limit, 2);
        assert_eq!(
            loaded
                .events
                .iter()
                .map(|event| event.sequence.value())
                .collect::<Vec<_>>(),
            vec![2, 3, 4]
        );
        assert_eq!(
            loaded
                .events
                .iter()
                .find(|event| event.session_id == session_id)
                .map(|event| event.sequence),
            Some(EventSequence::new(3))
        );

        // A small test-only aggregate budget exercises the same global policy
        // used in production without allocating a multi-megabyte fixture.
        let additional_events = DurableFeedState {
            next_sequence: EventSequence::new(6),
            retention_limit: 100,
            events: vec![
                ServerEventEnvelope {
                    protocol_version: loom_protocol::CURRENT_PROTOCOL_VERSION,
                    sequence: EventSequence::new(5),
                    session_id,
                    event: loom_protocol::ServerEvent::AgentSessionCreated {
                        snapshot: snapshot.clone(),
                    },
                },
                ServerEventEnvelope {
                    protocol_version: loom_protocol::CURRENT_PROTOCOL_VERSION,
                    sequence: EventSequence::new(6),
                    session_id: second_session_id,
                    event: loom_protocol::ServerEvent::AgentSessionCreated {
                        snapshot: AgentSessionSnapshot {
                            id: second_session_id,
                            workspace_id,
                            name: "Quiet feed".to_owned(),
                            state: AgentSessionState::Idle,
                            created_at: Timestamp::from_unix_millis(11),
                            updated_at: Timestamp::from_unix_millis(11),
                        },
                    },
                },
            ],
            workspace_events: Vec::new(),
        };
        let mut connection = Connection::open(&path).unwrap();
        let transaction = connection.transaction().unwrap();
        save_feed_rows_with_limits(&transaction, &additional_events, 1_000_000, 250).unwrap();
        transaction.commit().unwrap();
        let retained_sequences: Vec<i64> = {
            let mut statement = connection
                .prepare("SELECT sequence FROM feed_events ORDER BY sequence")
                .unwrap();
            statement
                .query_map([], |row| row.get(0))
                .unwrap()
                .collect::<std::result::Result<_, _>>()
                .unwrap()
        };
        assert!(retained_sequences.contains(&6));
        assert!(retained_sequences.iter().all(|sequence| *sequence >= 5));
        let retained_bytes: i64 = connection
            .query_row(
                "SELECT COALESCE(SUM(length(payload)), 0) FROM feed_events",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(retained_bytes <= 250);
        let quiet_cursor = store
            .load_feed_session_cursor(second_session_id)
            .unwrap()
            .unwrap();
        assert_eq!(quiet_cursor.latest_sequence, EventSequence::new(6));
        assert_eq!(quiet_cursor.pruned_through, EventSequence::new(2));
        drop(connection);

        let connection = Connection::open(&path).unwrap();
        let plan: String = connection
            .query_row(
                "EXPLAIN QUERY PLAN SELECT sequence FROM feed_events
                 WHERE session_id=?1 AND sequence>?2 ORDER BY sequence LIMIT 20",
                params![session_id.as_uuid().as_bytes().as_slice(), 0_i64],
                |row| row.get(3),
            )
            .unwrap();
        assert!(plan.contains("feed_events_by_session_sequence"), "{plan}");
        drop(connection);
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn workspace_reconnect_feed_is_isolated_indexed_and_tracks_pruning() {
        let path = std::env::temp_dir().join(format!("loom-persistence-{}.db", Uuid::new_v4()));
        let store = FilePersistence::open(&path).unwrap();
        let workspace_a = WorkspaceId::new();
        let workspace_b = WorkspaceId::new();
        let session_a = AgentSessionId::new();
        let session_b = AgentSessionId::new();
        let mut manager = SessionManager::default();
        manager
            .create_in_workspace_with_id(workspace_a, session_a, "Workspace A")
            .unwrap();
        manager
            .create_in_workspace_with_id(workspace_b, session_b, "Workspace B")
            .unwrap();
        let snapshot = |id, workspace_id| AgentSessionSnapshot {
            id,
            workspace_id,
            name: "session".to_owned(),
            state: AgentSessionState::Idle,
            created_at: Timestamp::from_unix_millis(1),
            updated_at: Timestamp::from_unix_millis(1),
        };
        let event = |sequence, id, workspace_id| ServerEventEnvelope {
            protocol_version: loom_protocol::CURRENT_PROTOCOL_VERSION,
            sequence: EventSequence::new(sequence),
            session_id: id,
            event: loom_protocol::ServerEvent::AgentSessionCreated {
                snapshot: snapshot(id, workspace_id),
            },
        };
        let feed = DurableFeedState {
            next_sequence: EventSequence::new(4),
            retention_limit: 1,
            events: vec![
                event(1, session_a, workspace_a),
                event(2, session_b, workspace_b),
                event(3, session_a, workspace_a),
                event(4, session_b, workspace_b),
            ],
            workspace_events: Vec::new(),
        };
        store
            .save_state_with_sessions_and_feed(&manager.export_state(), Some(&feed))
            .unwrap();

        let events_a = store
            .load_feed_workspace_events_since(workspace_a, None)
            .unwrap();
        let events_b = store
            .load_feed_workspace_events_since(workspace_b, None)
            .unwrap();
        assert_eq!(
            events_a
                .iter()
                .map(workspace_feed_event_sequence)
                .collect::<Vec<_>>(),
            vec![3]
        );
        assert_eq!(
            events_b
                .iter()
                .map(workspace_feed_event_sequence)
                .collect::<Vec<_>>(),
            vec![4]
        );
        let cursor_a = store
            .load_feed_workspace_cursor(workspace_a)
            .unwrap()
            .unwrap();
        let cursor_b = store
            .load_feed_workspace_cursor(workspace_b)
            .unwrap()
            .unwrap();
        assert_eq!(cursor_a.first_sequence, EventSequence::new(1));
        assert_eq!(cursor_a.latest_sequence, EventSequence::new(3));
        assert_eq!(cursor_a.pruned_through, EventSequence::new(1));
        assert_eq!(
            cursor_a.oldest_retained_sequence,
            Some(EventSequence::new(3))
        );
        assert_eq!(cursor_b.first_sequence, EventSequence::new(2));
        assert_eq!(cursor_b.latest_sequence, EventSequence::new(4));
        assert_eq!(cursor_b.pruned_through, EventSequence::new(2));
        assert_eq!(
            cursor_b.oldest_retained_sequence,
            Some(EventSequence::new(4))
        );

        let connection = Connection::open(&path).unwrap();
        let plan: String = connection
            .query_row(
                "EXPLAIN QUERY PLAN SELECT sequence FROM feed_events
             WHERE workspace_id=?1 AND sequence>?2 ORDER BY sequence LIMIT 20",
                params![workspace_a.as_uuid().as_bytes().as_slice(), 0_i64],
                |row| row.get(3),
            )
            .unwrap();
        assert!(plan.contains("feed_events_by_workspace_sequence"), "{plan}");
        drop(connection);
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn workspace_only_events_share_the_cursor_and_persist_with_bounded_retention() {
        let path = std::env::temp_dir().join(format!("loom-workspace-feed-{}.db", Uuid::new_v4()));
        let store = FilePersistence::open(&path).unwrap();
        let workspace_id = WorkspaceId::new();
        let workspace_event = |sequence, name: &str| WorkspaceEventEnvelope {
            protocol_version: loom_protocol::CURRENT_PROTOCOL_VERSION,
            sequence: EventSequence::new(sequence),
            workspace_id,
            event: loom_protocol::WorkspaceEvent::Renamed {
                name: name.to_owned(),
            },
        };
        let feed = DurableFeedState {
            next_sequence: EventSequence::new(2),
            retention_limit: 1,
            events: Vec::new(),
            workspace_events: vec![workspace_event(1, "First"), workspace_event(2, "Second")],
        };
        store
            .save_state_with_sessions_and_feed(
                &SessionManager::default().export_state(),
                Some(&feed),
            )
            .unwrap();
        let events = store
            .load_feed_workspace_events_since(workspace_id, None)
            .unwrap();
        assert_eq!(events.len(), 1);
        assert!(matches!(&events[0], WorkspaceFeedEvent::Workspace(event)
            if event.sequence == EventSequence::new(2)
                && matches!(&event.event, loom_protocol::WorkspaceEvent::Renamed { name } if name == "Second")));
        let cursor = store
            .load_feed_workspace_cursor(workspace_id)
            .unwrap()
            .unwrap();
        assert_eq!(cursor.latest_sequence, EventSequence::new(2));
        assert_eq!(cursor.pruned_through, EventSequence::new(1));
        assert_eq!(cursor.oldest_retained_sequence, Some(EventSequence::new(2)));
        let loaded = store.load_feed_state().unwrap().unwrap();
        assert_eq!(loaded.workspace_events.len(), 1);
        assert_eq!(loaded.workspace_events[0].sequence, EventSequence::new(2));
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn session_and_workspace_feed_share_the_global_payload_budget() {
        let path = std::env::temp_dir().join(format!("loom-mixed-feed-{}.db", Uuid::new_v4()));
        let store = FilePersistence::open(&path).unwrap();
        let workspace_id = WorkspaceId::new();
        let session_id = AgentSessionId::new();
        let mut manager = SessionManager::default();
        let (snapshot, _) = manager
            .create_in_workspace_with_id(workspace_id, session_id, "Feed budget")
            .unwrap();
        let empty_feed = DurableFeedState {
            next_sequence: EventSequence::default(),
            retention_limit: 100,
            events: Vec::new(),
            workspace_events: Vec::new(),
        };
        store
            .save_state_with_sessions_and_feed(&manager.export_state(), Some(&empty_feed))
            .unwrap();
        let feed = DurableFeedState {
            next_sequence: EventSequence::new(3),
            retention_limit: 100,
            events: vec![ServerEventEnvelope {
                protocol_version: loom_protocol::CURRENT_PROTOCOL_VERSION,
                sequence: EventSequence::new(1),
                session_id,
                event: loom_protocol::ServerEvent::AgentSessionCreated { snapshot },
            }],
            workspace_events: vec![
                WorkspaceEventEnvelope {
                    protocol_version: loom_protocol::CURRENT_PROTOCOL_VERSION,
                    sequence: EventSequence::new(2),
                    workspace_id,
                    event: loom_protocol::WorkspaceEvent::Renamed {
                        name: "workspace mutation one".to_owned(),
                    },
                },
                WorkspaceEventEnvelope {
                    protocol_version: loom_protocol::CURRENT_PROTOCOL_VERSION,
                    sequence: EventSequence::new(3),
                    workspace_id,
                    event: loom_protocol::WorkspaceEvent::Renamed {
                        name: "workspace mutation two".to_owned(),
                    },
                },
            ],
        };
        let mut connection = Connection::open(&path).unwrap();
        let transaction = connection.transaction().unwrap();
        save_feed_rows_with_limits(&transaction, &feed, 10_000, 250).unwrap();
        transaction.commit().unwrap();
        let retained_bytes: i64 = connection
            .query_row(
                "SELECT COALESCE((SELECT SUM(length(payload)) FROM feed_events), 0)
                      + COALESCE((SELECT SUM(length(payload)) FROM workspace_feed_events), 0)",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(
            retained_bytes <= 250,
            "retained payload bytes: {retained_bytes}"
        );
        drop(connection);
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn reconnect_event_decoder_accepts_compressed_rows_and_rejects_corruption() {
        let session_id = AgentSessionId::new();
        let event = ServerEventEnvelope {
            protocol_version: loom_protocol::CURRENT_PROTOCOL_VERSION,
            sequence: EventSequence::new(7),
            session_id,
            event: loom_protocol::ServerEvent::AgentSessionCreated {
                snapshot: AgentSessionSnapshot {
                    id: session_id,
                    workspace_id: WorkspaceId::new(),
                    name: "compressed event".to_owned(),
                    state: AgentSessionState::Idle,
                    created_at: Timestamp::from_unix_millis(1),
                    updated_at: Timestamp::from_unix_millis(1),
                },
            },
        };
        let raw = serde_json::to_vec(&event).unwrap();
        let mut encoder = ZlibEncoder::new(Vec::new(), Compression::fast());
        encoder.write_all(&raw).unwrap();
        let compressed = encoder.finish().unwrap();
        assert!(compressed.len() < raw.len());
        assert_eq!(
            decode_feed_event(
                7,
                session_id.as_uuid().as_bytes().to_vec(),
                1,
                compressed.clone(),
            )
            .unwrap(),
            event
        );
        assert_eq!(
            decode_feed_event(
                7,
                session_id.as_uuid().as_bytes().to_vec(),
                99,
                compressed.clone(),
            )
            .unwrap_err()
            .code,
            ErrorCode::MalformedPayload
        );
        assert_eq!(
            decode_feed_event(
                7,
                session_id.as_uuid().as_bytes().to_vec(),
                1,
                b"not a zlib stream".to_vec(),
            )
            .unwrap_err()
            .code,
            ErrorCode::MalformedPayload
        );
        assert_eq!(
            decode_feed_event(8, session_id.as_uuid().as_bytes().to_vec(), 1, compressed,)
                .unwrap_err()
                .code,
            ErrorCode::MalformedPayload
        );
    }
}
