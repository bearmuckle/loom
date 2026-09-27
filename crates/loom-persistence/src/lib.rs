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
    ActivityId, AgentSessionId, AgentSessionSnapshot, AgentSessionState, ApprovalPolicy,
    CheckpointId, ErrorCode, EventSequence, InteractionId, LoomError, PolicyDecision, RepositoryId,
    RequestId, Result, RunAttemptId, RunId, SessionLimits, StepId, Timestamp, UsageSnapshot,
    WorkspaceId, WorkspaceRecord,
};
use loom_model::{ModelId, ProviderHealth, ProviderId, ProviderUsageSummary};
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
use loom_providers::{ProviderConfig, ProviderUsageKey, UsageLedger};
use loom_session::{SessionManagerState, WorkspaceManagerState};
use rusqlite::{Connection, OptionalExtension, Transaction, params, types::Value as SqlValue};
use serde::{Serialize, de::DeserializeOwned};
use serde_json::Value;
use sha2::{Digest, Sha256};
use uuid::Uuid;

const DATABASE_SCHEMA_VERSION: u32 = 41;
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

const DATABASE_SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS session_store_meta (
    singleton INTEGER PRIMARY KEY CHECK(singleton = 1),
    next_sequence INTEGER NOT NULL CHECK(next_sequence >= 0)
) STRICT;
CREATE TABLE IF NOT EXISTS sessions (
    id BLOB PRIMARY KEY NOT NULL CHECK(length(id) = 16),
    workspace_id BLOB NOT NULL CHECK(length(workspace_id) = 16),
    name TEXT NOT NULL CHECK(length(trim(name)) > 0),
    state TEXT NOT NULL CHECK(state IN (
        'idle', 'queued', 'planning', 'awaiting_approval', 'paused', 'executing',
        'evaluating', 'needs_input', 'completed', 'failed', 'cancelled', 'archived'
    )),
    created_at INTEGER NOT NULL CHECK(created_at >= 0),
    updated_at INTEGER NOT NULL CHECK(updated_at >= created_at)
) WITHOUT ROWID, STRICT;
CREATE INDEX IF NOT EXISTS sessions_visible
    ON sessions(workspace_id, updated_at DESC, id DESC) WHERE state != 'archived';
CREATE INDEX IF NOT EXISTS sessions_archived
    ON sessions(workspace_id, updated_at DESC, id DESC) WHERE state = 'archived';
CREATE TABLE IF NOT EXISTS run_summaries (
    run_id BLOB PRIMARY KEY NOT NULL CHECK(length(run_id) = 16),
    session_id BLOB NOT NULL REFERENCES sessions(id) ON DELETE CASCADE CHECK(length(session_id) = 16),
    attempt_id BLOB NOT NULL CHECK(length(attempt_id) = 16),
    control_revision INTEGER NOT NULL CHECK(control_revision >= 0),
    state TEXT NOT NULL CHECK(state IN (
        'planning', 'executing', 'awaiting_approval', 'paused', 'needs_input',
        'evaluating', 'completed', 'failed', 'cancelled'
    )),
    started_at INTEGER NOT NULL CHECK(started_at >= 0),
    updated_at INTEGER NOT NULL CHECK(updated_at >= started_at),
    completed_at INTEGER CHECK(completed_at IS NULL OR completed_at >= started_at),
    task TEXT NOT NULL CHECK(length(task) <= 1048576),
    model TEXT NOT NULL CHECK(length(model) <= 256),
    summary TEXT CHECK(summary IS NULL OR length(summary) <= 1048576),
    input_tokens INTEGER NOT NULL CHECK(input_tokens >= 0),
    output_tokens INTEGER NOT NULL CHECK(output_tokens >= 0),
    cached_input_tokens INTEGER NOT NULL CHECK(cached_input_tokens >= 0),
    tool_calls INTEGER NOT NULL CHECK(tool_calls >= 0),
    cost_micros INTEGER NOT NULL CHECK(cost_micros >= 0),
    elapsed_ms INTEGER NOT NULL CHECK(elapsed_ms >= 0),
    UNIQUE(run_id, session_id)
) WITHOUT ROWID, STRICT;
CREATE INDEX IF NOT EXISTS runs_by_session_activity
    ON run_summaries(session_id, updated_at DESC, run_id DESC);
CREATE INDEX IF NOT EXISTS runs_by_state_activity
    ON run_summaries(state, updated_at DESC, run_id DESC);
CREATE TABLE IF NOT EXISTS runtime_configurations (
    configuration_hash BLOB PRIMARY KEY NOT NULL CHECK(length(configuration_hash) = 32),
    system_instructions_hash BLOB REFERENCES content_objects(hash) ON DELETE RESTRICT
        CHECK(system_instructions_hash IS NULL OR length(system_instructions_hash) = 32),
    repository_instructions_hash BLOB REFERENCES content_objects(hash) ON DELETE RESTRICT
        CHECK(repository_instructions_hash IS NULL OR length(repository_instructions_hash) = 32),
    policy_read TEXT NOT NULL CHECK(policy_read IN ('allow', 'require_approval', 'deny')),
    policy_write TEXT NOT NULL CHECK(policy_write IN ('allow', 'require_approval', 'deny')),
    policy_command TEXT NOT NULL CHECK(policy_command IN ('allow', 'require_approval', 'deny')),
    policy_network TEXT NOT NULL CHECK(policy_network IN ('allow', 'require_approval', 'deny')),
    policy_destructive TEXT NOT NULL CHECK(policy_destructive IN ('allow', 'require_approval', 'deny')),
    max_duration_ms INTEGER CHECK(max_duration_ms IS NULL OR max_duration_ms >= 0),
    max_input_tokens INTEGER CHECK(max_input_tokens IS NULL OR max_input_tokens >= 0),
    max_output_tokens INTEGER CHECK(max_output_tokens IS NULL OR max_output_tokens >= 0),
    max_tool_calls INTEGER CHECK(max_tool_calls IS NULL OR max_tool_calls >= 0),
    max_cost_micros INTEGER CHECK(max_cost_micros IS NULL OR max_cost_micros >= 0),
    context_window INTEGER CHECK(context_window IS NULL OR context_window >= 0),
    context_max_input_tokens INTEGER CHECK(context_max_input_tokens IS NULL OR context_max_input_tokens >= 0),
    context_reserved_output_tokens INTEGER CHECK(context_reserved_output_tokens IS NULL OR context_reserved_output_tokens >= 0),
    checkpoint_id BLOB CHECK(checkpoint_id IS NULL OR length(checkpoint_id) = 16),
    input_cost_micros_per_1k INTEGER NOT NULL CHECK(input_cost_micros_per_1k >= 0),
    output_cost_micros_per_1k INTEGER NOT NULL CHECK(output_cost_micros_per_1k >= 0)
) WITHOUT ROWID, STRICT;
CREATE TABLE IF NOT EXISTS run_runtime_config (
    run_id BLOB PRIMARY KEY NOT NULL REFERENCES run_summaries(run_id) ON DELETE CASCADE,
    configuration_hash BLOB NOT NULL REFERENCES runtime_configurations(configuration_hash) ON DELETE RESTRICT
        CHECK(length(configuration_hash) = 32),
    context_inspection TEXT
) WITHOUT ROWID, STRICT;
CREATE INDEX IF NOT EXISTS run_runtime_config_by_configuration
    ON run_runtime_config(configuration_hash);
CREATE TABLE IF NOT EXISTS run_plan_steps (
    run_id BLOB NOT NULL,
    session_id BLOB NOT NULL CHECK(length(session_id) = 16),
    ordinal INTEGER NOT NULL CHECK(ordinal >= 0),
    step_id TEXT NOT NULL CHECK(length(step_id) <= 256),
    description TEXT NOT NULL CHECK(length(description) <= 16384),
    PRIMARY KEY(run_id, ordinal),
    FOREIGN KEY(run_id, session_id)
        REFERENCES run_summaries(run_id, session_id) ON DELETE CASCADE
) WITHOUT ROWID, STRICT;
CREATE TABLE IF NOT EXISTS run_evidence (
    run_id BLOB NOT NULL,
    session_id BLOB NOT NULL CHECK(length(session_id) = 16),
    ordinal INTEGER NOT NULL CHECK(ordinal >= 0),
    label TEXT NOT NULL CHECK(length(label) <= 16384),
    uri TEXT NOT NULL CHECK(length(uri) <= 16384),
    PRIMARY KEY(run_id, ordinal),
    FOREIGN KEY(run_id, session_id)
        REFERENCES run_summaries(run_id, session_id) ON DELETE CASCADE
) WITHOUT ROWID, STRICT;
CREATE TABLE IF NOT EXISTS run_attempts (
    run_id BLOB NOT NULL,
    session_id BLOB NOT NULL CHECK(length(session_id) = 16),
    attempt_id BLOB NOT NULL CHECK(length(attempt_id) = 16),
    attempt_number INTEGER NOT NULL CHECK(attempt_number > 0),
    state TEXT NOT NULL CHECK(state IN (
        'planning', 'executing', 'awaiting_approval', 'paused', 'needs_input',
        'evaluating', 'completed', 'failed', 'cancelled'
    )),
    checkpoint_id BLOB CHECK(checkpoint_id IS NULL OR length(checkpoint_id) = 16),
    started_at INTEGER NOT NULL CHECK(started_at >= 0),
    completed_at INTEGER CHECK(completed_at IS NULL OR completed_at >= started_at),
    PRIMARY KEY(run_id, attempt_id),
    UNIQUE(run_id, attempt_number),
    FOREIGN KEY(run_id, session_id)
        REFERENCES run_summaries(run_id, session_id) ON DELETE CASCADE
) WITHOUT ROWID, STRICT;
CREATE INDEX IF NOT EXISTS run_attempts_by_session
    ON run_attempts(session_id, run_id, attempt_number DESC);
CREATE TABLE IF NOT EXISTS run_execution_state (
    run_id BLOB PRIMARY KEY NOT NULL,
    session_id BLOB NOT NULL CHECK(length(session_id) = 16),
    attempt_id BLOB NOT NULL CHECK(length(attempt_id) = 16),
    control_revision INTEGER NOT NULL CHECK(control_revision >= 0),
    state TEXT NOT NULL CHECK(state IN (
        'planning', 'executing', 'awaiting_approval', 'paused', 'needs_input',
        'evaluating', 'completed', 'failed', 'cancelled'
    )),
    step_id BLOB CHECK(step_id IS NULL OR length(step_id) = 16),
    step_index INTEGER NOT NULL CHECK(step_index >= 0),
    provider_cursor INTEGER NOT NULL CHECK(provider_cursor >= 0),
    next_message_id INTEGER NOT NULL CHECK(next_message_id >= 0),
    active_message_id INTEGER CHECK(active_message_id IS NULL OR active_message_id >= 0),
    pending_tool_execution TEXT CHECK(
        pending_tool_execution IS NULL OR length(pending_tool_execution) <= 1048576
    ),
    pending_approval TEXT CHECK(
        pending_approval IS NULL OR length(pending_approval) <= 1048576
    ),
    pending_input TEXT CHECK(pending_input IS NULL OR length(pending_input) <= 65536),
    last_failed_call TEXT CHECK(last_failed_call IS NULL OR length(last_failed_call) <= 1048576),
    FOREIGN KEY(run_id, session_id)
        REFERENCES run_summaries(run_id, session_id) ON DELETE CASCADE,
    FOREIGN KEY(run_id, attempt_id)
        REFERENCES run_attempts(run_id, attempt_id) ON DELETE CASCADE,
    CHECK(pending_tool_execution IS NULL OR state IN ('executing', 'evaluating')),
    CHECK(pending_approval IS NULL OR state IN ('awaiting_approval', 'paused')),
    CHECK(pending_input IS NULL OR state IN ('needs_input', 'paused'))
) WITHOUT ROWID, STRICT;
CREATE INDEX IF NOT EXISTS run_execution_state_by_session
    ON run_execution_state(session_id, state, run_id);
CREATE TABLE IF NOT EXISTS run_interactions (
    run_id BLOB NOT NULL,
    session_id BLOB NOT NULL CHECK(length(session_id) = 16),
    interaction_id BLOB NOT NULL CHECK(length(interaction_id) = 16),
    attempt_id BLOB NOT NULL CHECK(length(attempt_id) = 16),
    control_revision INTEGER NOT NULL CHECK(control_revision >= 0),
    kind TEXT NOT NULL CHECK(kind IN ('tool_approval', 'user_input')),
    status TEXT NOT NULL CHECK(status IN (
        'pending', 'approved', 'rejected', 'answered', 'abandoned'
    )),
    tool_call_id BLOB CHECK(tool_call_id IS NULL OR length(tool_call_id) = 16),
    prompt TEXT NOT NULL CHECK(length(prompt) <= 65536),
    decision TEXT CHECK(decision IS NULL OR decision IN ('approved', 'rejected')),
    created_at INTEGER NOT NULL CHECK(created_at >= 0),
    resolved_at INTEGER CHECK(resolved_at IS NULL OR resolved_at >= created_at),
    PRIMARY KEY(run_id, interaction_id),
    UNIQUE(run_id, attempt_id, control_revision),
    FOREIGN KEY(run_id, session_id)
        REFERENCES run_summaries(run_id, session_id) ON DELETE CASCADE,
    CHECK((kind = 'tool_approval' AND tool_call_id IS NOT NULL)
        OR (kind = 'user_input' AND tool_call_id IS NULL)),
    CHECK((status = 'pending' AND resolved_at IS NULL AND decision IS NULL)
        OR (status = 'approved' AND resolved_at IS NOT NULL AND decision = 'approved')
        OR (status = 'rejected' AND resolved_at IS NOT NULL AND decision = 'rejected')
        OR (status IN ('answered', 'abandoned') AND resolved_at IS NOT NULL AND decision IS NULL)),
    CHECK(kind = 'tool_approval' OR status NOT IN ('approved', 'rejected')),
    CHECK(kind = 'user_input' OR status != 'answered')
) WITHOUT ROWID, STRICT;
CREATE INDEX IF NOT EXISTS run_interactions_pending
    ON run_interactions(session_id, created_at, interaction_id) WHERE status = 'pending';
CREATE INDEX IF NOT EXISTS run_interactions_by_attempt
    ON run_interactions(run_id, attempt_id, control_revision);
CREATE TABLE IF NOT EXISTS run_messages (
    run_id BLOB NOT NULL REFERENCES run_summaries(run_id) ON DELETE CASCADE
        CHECK(length(run_id) = 16),
    session_id BLOB NOT NULL REFERENCES sessions(id) ON DELETE CASCADE
        CHECK(length(session_id) = 16),
    ordinal INTEGER NOT NULL CHECK(ordinal >= 0),
    role TEXT NOT NULL CHECK(role IN ('system', 'user', 'assistant', 'tool')),
    content_hash BLOB REFERENCES content_objects(hash) ON DELETE RESTRICT
        CHECK(content_hash IS NULL OR length(content_hash) = 32),
    name TEXT,
    tool_call_id BLOB CHECK(tool_call_id IS NULL OR length(tool_call_id) = 16),
    PRIMARY KEY(run_id, ordinal),
    FOREIGN KEY(run_id, session_id)
        REFERENCES run_summaries(run_id, session_id) ON DELETE CASCADE
) WITHOUT ROWID, STRICT;
CREATE TABLE IF NOT EXISTS run_message_tool_calls (
    run_id BLOB NOT NULL,
    message_ordinal INTEGER NOT NULL CHECK(message_ordinal >= 0),
    call_ordinal INTEGER NOT NULL CHECK(call_ordinal >= 0),
    tool_call_id BLOB NOT NULL CHECK(length(tool_call_id) = 16),
    name TEXT NOT NULL CHECK(length(name) <= 4096),
    arguments_hash BLOB NOT NULL REFERENCES content_objects(hash) ON DELETE RESTRICT
        CHECK(length(arguments_hash) = 32),
    PRIMARY KEY(run_id, message_ordinal, call_ordinal),
    UNIQUE(run_id, message_ordinal, tool_call_id),
    FOREIGN KEY(run_id, message_ordinal)
        REFERENCES run_messages(run_id, ordinal) ON DELETE CASCADE
) WITHOUT ROWID, STRICT;
CREATE INDEX IF NOT EXISTS run_message_tool_calls_by_id
    ON run_message_tool_calls(run_id, tool_call_id);
CREATE TABLE IF NOT EXISTS run_context_checkpoints (
    run_id BLOB PRIMARY KEY NOT NULL CHECK(length(run_id) = 16),
    session_id BLOB NOT NULL CHECK(length(session_id) = 16),
    summary_hash BLOB NOT NULL REFERENCES content_objects(hash) ON DELETE RESTRICT
        CHECK(length(summary_hash) = 32),
    source_message_count INTEGER NOT NULL CHECK(source_message_count >= 0),
    projection_version INTEGER NOT NULL CHECK(projection_version >= 0),
    source_digest TEXT NOT NULL CHECK(length(source_digest) <= 64),
    created_at INTEGER NOT NULL CHECK(created_at >= 0),
    FOREIGN KEY(run_id, session_id)
        REFERENCES run_summaries(run_id, session_id) ON DELETE CASCADE
) WITHOUT ROWID, STRICT;
CREATE INDEX IF NOT EXISTS run_context_checkpoints_by_session
    ON run_context_checkpoints(session_id, run_id);
CREATE INDEX IF NOT EXISTS run_messages_by_session
    ON run_messages(session_id, run_id, ordinal);
CREATE TABLE IF NOT EXISTS run_activities (
    run_id BLOB NOT NULL,
    session_id BLOB NOT NULL CHECK(length(session_id) = 16),
    activity_id BLOB NOT NULL CHECK(length(activity_id) = 16),
    ordinal INTEGER NOT NULL CHECK(ordinal >= 0),
    parent_activity_id BLOB CHECK(parent_activity_id IS NULL OR length(parent_activity_id) = 16),
    step_id BLOB CHECK(step_id IS NULL OR length(step_id) = 16),
    tool_call_id BLOB CHECK(tool_call_id IS NULL OR length(tool_call_id) = 16),
    kind TEXT NOT NULL CHECK(kind IN ('model_turn', 'tool_call', 'file', 'search', 'command')),
    status TEXT NOT NULL CHECK(status IN (
        'started', 'completed', 'failed', 'awaiting_approval', 'awaiting_input', 'cancelled'
    )),
    started_at INTEGER NOT NULL CHECK(started_at >= 0),
    completed_at INTEGER CHECK(completed_at IS NULL OR completed_at >= started_at),
    elapsed_ms INTEGER CHECK(elapsed_ms IS NULL OR elapsed_ms >= 0),
    data_hash BLOB NOT NULL REFERENCES content_objects(hash) ON DELETE RESTRICT
        CHECK(length(data_hash) = 32),
    PRIMARY KEY(run_id, activity_id),
    UNIQUE(run_id, ordinal),
    FOREIGN KEY(run_id, session_id)
        REFERENCES run_summaries(run_id, session_id) ON DELETE CASCADE
) WITHOUT ROWID, STRICT;
CREATE INDEX IF NOT EXISTS run_activities_by_session_time
    ON run_activities(session_id, started_at DESC, activity_id DESC);
CREATE INDEX IF NOT EXISTS run_activities_by_tool_call
    ON run_activities(run_id, tool_call_id, ordinal) WHERE tool_call_id IS NOT NULL;
CREATE TABLE IF NOT EXISTS run_tool_calls (
    run_id BLOB NOT NULL,
    session_id BLOB NOT NULL CHECK(length(session_id) = 16),
    tool_call_id BLOB NOT NULL CHECK(length(tool_call_id) = 16),
    name TEXT NOT NULL CHECK(length(name) <= 4096),
    arguments_hash BLOB NOT NULL REFERENCES content_objects(hash) ON DELETE RESTRICT
        CHECK(length(arguments_hash) = 32),
    created_at INTEGER NOT NULL CHECK(created_at >= 0),
    PRIMARY KEY(run_id, tool_call_id),
    FOREIGN KEY(run_id, session_id)
        REFERENCES run_summaries(run_id, session_id) ON DELETE CASCADE
) WITHOUT ROWID, STRICT;
CREATE TABLE IF NOT EXISTS run_tool_attempts (
    run_id BLOB NOT NULL,
    session_id BLOB NOT NULL CHECK(length(session_id) = 16),
    activity_id BLOB NOT NULL CHECK(length(activity_id) = 16),
    tool_call_id BLOB NOT NULL CHECK(length(tool_call_id) = 16),
    attempt_number INTEGER NOT NULL CHECK(attempt_number > 0),
    state TEXT NOT NULL CHECK(state IN (
        'queued', 'running', 'awaiting_approval', 'awaiting_input',
        'completed', 'failed', 'cancelled', 'outcome_unknown'
    )),
    started_at INTEGER NOT NULL CHECK(started_at >= 0),
    completed_at INTEGER CHECK(completed_at IS NULL OR completed_at >= started_at),
    result_hash BLOB REFERENCES content_objects(hash) ON DELETE RESTRICT
        CHECK(result_hash IS NULL OR length(result_hash) = 32),
    PRIMARY KEY(run_id, activity_id),
    UNIQUE(run_id, tool_call_id, attempt_number),
    FOREIGN KEY(run_id, session_id)
        REFERENCES run_summaries(run_id, session_id) ON DELETE CASCADE,
    FOREIGN KEY(run_id, tool_call_id)
        REFERENCES run_tool_calls(run_id, tool_call_id) ON DELETE CASCADE,
    FOREIGN KEY(run_id, activity_id)
        REFERENCES run_activities(run_id, activity_id) ON DELETE CASCADE
) WITHOUT ROWID, STRICT;
CREATE INDEX IF NOT EXISTS run_tool_attempts_by_call
    ON run_tool_attempts(run_id, tool_call_id, attempt_number);
CREATE TABLE IF NOT EXISTS run_message_fragments (
    run_id BLOB NOT NULL,
    message_ordinal INTEGER NOT NULL CHECK(message_ordinal >= 0),
    fragment_ordinal INTEGER NOT NULL CHECK(fragment_ordinal >= 0),
    byte_offset INTEGER NOT NULL CHECK(byte_offset >= 0),
    byte_length INTEGER NOT NULL CHECK(byte_length > 0 AND byte_length <= 32768),
    content_hash BLOB NOT NULL REFERENCES content_objects(hash) ON DELETE RESTRICT
        CHECK(length(content_hash) = 32),
    PRIMARY KEY(run_id, message_ordinal, fragment_ordinal),
    UNIQUE(run_id, message_ordinal, byte_offset),
    FOREIGN KEY(run_id, message_ordinal)
        REFERENCES run_messages(run_id, ordinal) ON DELETE CASCADE
) WITHOUT ROWID, STRICT;
CREATE INDEX IF NOT EXISTS run_message_fragments_by_range
    ON run_message_fragments(run_id, message_ordinal, byte_offset);
CREATE TABLE IF NOT EXISTS session_filesystems (
    session_id BLOB PRIMARY KEY NOT NULL REFERENCES sessions(id) ON DELETE CASCADE
        CHECK(length(session_id) = 16),
    root TEXT NOT NULL,
    control TEXT NOT NULL CHECK(control IN ('agent', 'user')),
    payload_hash BLOB NOT NULL CHECK(length(payload_hash) = 32),
    raw_size INTEGER NOT NULL CHECK(raw_size >= 0 AND raw_size <= 536870912),
    payload_codec INTEGER NOT NULL CHECK(payload_codec IN (0, 1)),
    payload BLOB NOT NULL CHECK(length(payload) <= 536870912)
) WITHOUT ROWID, STRICT;
CREATE TABLE IF NOT EXISTS filesystem_change_state (
    session_id BLOB PRIMARY KEY NOT NULL REFERENCES session_filesystems(session_id) ON DELETE CASCADE
        CHECK(length(session_id) = 16),
    next_sequence INTEGER NOT NULL CHECK(next_sequence >= 0)
) WITHOUT ROWID, STRICT;
CREATE TABLE IF NOT EXISTS filesystem_edits (
    session_id BLOB NOT NULL REFERENCES session_filesystems(session_id) ON DELETE CASCADE
        CHECK(length(session_id) = 16),
    edit_id INTEGER NOT NULL CHECK(edit_id > 0),
    path TEXT NOT NULL CHECK(length(path) > 0 AND length(path) <= 4096),
    before_hash BLOB REFERENCES content_objects(hash) ON DELETE RESTRICT
        CHECK(before_hash IS NULL OR length(before_hash) = 32),
    after_revision TEXT NOT NULL CHECK(length(after_revision) <= 256),
    source TEXT NOT NULL CHECK(source IN ('agent', 'user')),
    PRIMARY KEY(session_id, edit_id)
) WITHOUT ROWID, STRICT;
CREATE TABLE IF NOT EXISTS filesystem_changes (
    session_id BLOB NOT NULL REFERENCES session_filesystems(session_id) ON DELETE CASCADE
        CHECK(length(session_id) = 16),
    sequence INTEGER NOT NULL CHECK(sequence >= 0),
    path TEXT NOT NULL CHECK(length(path) > 0 AND length(path) <= 4096),
    kind TEXT NOT NULL CHECK(kind IN ('created', 'modified', 'deleted')),
    revision TEXT CHECK(revision IS NULL OR length(revision) <= 256),
    PRIMARY KEY(session_id, sequence)
) WITHOUT ROWID, STRICT;
CREATE TABLE IF NOT EXISTS session_repositories (
    session_id BLOB NOT NULL REFERENCES session_filesystems(session_id) ON DELETE CASCADE
        CHECK(length(session_id) = 16),
    repository_id BLOB NOT NULL CHECK(length(repository_id) = 16),
    source TEXT NOT NULL,
    path TEXT NOT NULL CHECK(length(path) > 0 AND length(path) <= 4096),
    revision TEXT,
    attached_at INTEGER NOT NULL CHECK(attached_at >= 0),
    PRIMARY KEY(session_id, repository_id)
) WITHOUT ROWID, STRICT;
CREATE INDEX IF NOT EXISTS session_repositories_by_path
    ON session_repositories(session_id, path, repository_id);
CREATE TABLE IF NOT EXISTS session_directories (
    session_id BLOB NOT NULL REFERENCES session_filesystems(session_id) ON DELETE CASCADE
        CHECK(length(session_id) = 16),
    ordinal INTEGER NOT NULL CHECK(ordinal >= 0),
    source TEXT NOT NULL,
    path TEXT NOT NULL CHECK(length(path) > 0 AND length(path) <= 4096),
    PRIMARY KEY(session_id, ordinal),
    UNIQUE(session_id, path)
) WITHOUT ROWID, STRICT;
CREATE INDEX IF NOT EXISTS filesystem_edits_by_content
    ON filesystem_edits(before_hash) WHERE before_hash IS NOT NULL;
CREATE TABLE IF NOT EXISTS checkpoints (
    session_id BLOB NOT NULL REFERENCES session_filesystems(session_id) ON DELETE CASCADE
        CHECK(length(session_id) = 16),
    checkpoint_id BLOB NOT NULL CHECK(length(checkpoint_id) = 16),
    label TEXT NOT NULL CHECK(length(trim(label)) > 0 AND length(label) <= 16384),
    created_at INTEGER NOT NULL CHECK(created_at >= 0),
    PRIMARY KEY(session_id, checkpoint_id)
) WITHOUT ROWID, STRICT;
CREATE INDEX IF NOT EXISTS checkpoints_by_session_time
    ON checkpoints(session_id, created_at DESC, checkpoint_id DESC);
CREATE TABLE IF NOT EXISTS checkpoint_files (
    session_id BLOB NOT NULL,
    checkpoint_id BLOB NOT NULL,
    path TEXT NOT NULL CHECK(length(trim(path)) > 0),
    existed INTEGER NOT NULL CHECK(existed IN (0, 1)),
    revision TEXT NOT NULL,
    expected_revision TEXT NOT NULL,
    content_hash BLOB NOT NULL REFERENCES content_objects(hash) ON DELETE RESTRICT
        CHECK(length(content_hash) = 32),
    PRIMARY KEY(session_id, checkpoint_id, path),
    FOREIGN KEY(session_id, checkpoint_id)
        REFERENCES checkpoints(session_id, checkpoint_id) ON DELETE CASCADE
) WITHOUT ROWID, STRICT;
CREATE INDEX IF NOT EXISTS checkpoint_files_by_content
    ON checkpoint_files(content_hash);
CREATE TABLE IF NOT EXISTS workspaces (
    id BLOB PRIMARY KEY NOT NULL CHECK(length(id) = 16),
    name TEXT NOT NULL CHECK(length(trim(name)) > 0),
    created_at INTEGER NOT NULL CHECK(created_at >= 0),
    updated_at INTEGER NOT NULL CHECK(updated_at >= created_at)
) WITHOUT ROWID, STRICT;
CREATE INDEX IF NOT EXISTS workspaces_by_activity
    ON workspaces(updated_at DESC, id DESC);
CREATE TABLE IF NOT EXISTS session_settings (
    session_id BLOB PRIMARY KEY NOT NULL CHECK(length(session_id) = 16),
    policy_read TEXT NOT NULL CHECK(policy_read IN ('allow', 'require_approval', 'deny')),
    policy_write TEXT NOT NULL CHECK(policy_write IN ('allow', 'require_approval', 'deny')),
    policy_command TEXT NOT NULL CHECK(policy_command IN ('allow', 'require_approval', 'deny')),
    policy_network TEXT NOT NULL CHECK(policy_network IN ('allow', 'require_approval', 'deny')),
    policy_destructive TEXT NOT NULL CHECK(policy_destructive IN ('allow', 'require_approval', 'deny')),
    auto_approve_actions INTEGER NOT NULL CHECK(auto_approve_actions IN (0, 1))
) WITHOUT ROWID, STRICT;
CREATE TABLE IF NOT EXISTS workspace_configs (
    workspace_id BLOB PRIMARY KEY NOT NULL CHECK(length(workspace_id) = 16),
    revision INTEGER NOT NULL CHECK(revision >= 0),
    config TEXT NOT NULL CHECK(length(config) <= 16384)
) WITHOUT ROWID, STRICT;
CREATE INDEX IF NOT EXISTS workspace_configs_by_revision
    ON workspace_configs(revision DESC, workspace_id);
CREATE TABLE IF NOT EXISTS provider_configs (
    provider_id TEXT PRIMARY KEY NOT NULL CHECK(length(trim(provider_id)) > 0),
    config TEXT NOT NULL CHECK(length(config) <= 65536)
) WITHOUT ROWID, STRICT;
CREATE TABLE IF NOT EXISTS provider_health (
    provider_id TEXT PRIMARY KEY NOT NULL CHECK(length(trim(provider_id)) > 0),
    health TEXT NOT NULL CHECK(length(health) <= 16384)
) WITHOUT ROWID, STRICT;
CREATE TABLE IF NOT EXISTS provider_usage_totals (
    provider_id TEXT NOT NULL CHECK(length(trim(provider_id)) > 0),
    model_id TEXT NOT NULL CHECK(length(trim(model_id)) > 0),
    requests INTEGER NOT NULL CHECK(requests >= 0),
    input_tokens INTEGER NOT NULL CHECK(input_tokens >= 0),
    output_tokens INTEGER NOT NULL CHECK(output_tokens >= 0),
    cached_input_tokens INTEGER NOT NULL CHECK(cached_input_tokens >= 0),
    cost_micros INTEGER NOT NULL CHECK(cost_micros >= 0),
    PRIMARY KEY(provider_id, model_id)
) WITHOUT ROWID, STRICT;
CREATE TABLE IF NOT EXISTS idempotency_records (
    request_id BLOB PRIMARY KEY NOT NULL CHECK(length(request_id) = 16),
    created_at INTEGER NOT NULL CHECK(created_at >= 0),
    expires_at INTEGER CHECK(expires_at IS NULL OR expires_at >= created_at),
    request_hash BLOB NOT NULL CHECK(length(request_hash) = 32),
    request TEXT NOT NULL CHECK(length(request) <= 1048576),
    response TEXT NOT NULL CHECK(length(response) <= 1048576)
) WITHOUT ROWID, STRICT;
CREATE INDEX IF NOT EXISTS idempotency_expiry
    ON idempotency_records(expires_at, request_id)
    WHERE expires_at IS NOT NULL;
CREATE TABLE IF NOT EXISTS feed_store_meta (
    singleton INTEGER PRIMARY KEY CHECK(singleton = 1),
    next_sequence INTEGER NOT NULL CHECK(next_sequence >= 0),
    retention_limit INTEGER NOT NULL CHECK(retention_limit >= 0)
) STRICT;
CREATE TABLE IF NOT EXISTS feed_events (
    sequence INTEGER PRIMARY KEY CHECK(sequence > 0),
    session_id BLOB NOT NULL CHECK(length(session_id) = 16),
    workspace_id BLOB NOT NULL CHECK(length(workspace_id) = 16),
    payload_codec INTEGER NOT NULL CHECK(payload_codec IN (0, 1)),
    payload BLOB NOT NULL
) STRICT;
CREATE INDEX IF NOT EXISTS feed_events_by_session_sequence
    ON feed_events(session_id, sequence);
CREATE INDEX IF NOT EXISTS feed_events_by_workspace_sequence
    ON feed_events(workspace_id, sequence);
CREATE TABLE IF NOT EXISTS workspace_feed_events (
    sequence INTEGER PRIMARY KEY CHECK(sequence > 0),
    workspace_id BLOB NOT NULL CHECK(length(workspace_id) = 16),
    payload_codec INTEGER NOT NULL CHECK(payload_codec IN (0, 1)),
    payload BLOB NOT NULL
) STRICT;
CREATE INDEX IF NOT EXISTS workspace_feed_events_by_workspace_sequence
    ON workspace_feed_events(workspace_id, sequence);
CREATE TABLE IF NOT EXISTS feed_session_meta (
    session_id BLOB PRIMARY KEY NOT NULL CHECK(length(session_id) = 16),
    first_sequence INTEGER NOT NULL CHECK(first_sequence > 0),
    latest_sequence INTEGER NOT NULL CHECK(latest_sequence >= first_sequence),
    pruned_through INTEGER NOT NULL CHECK(pruned_through >= 0)
) WITHOUT ROWID, STRICT;
CREATE TABLE IF NOT EXISTS feed_workspace_meta (
    workspace_id BLOB PRIMARY KEY NOT NULL CHECK(length(workspace_id) = 16),
    first_sequence INTEGER NOT NULL CHECK(first_sequence > 0),
    latest_sequence INTEGER NOT NULL CHECK(latest_sequence >= first_sequence),
    pruned_through INTEGER NOT NULL CHECK(pruned_through >= 0)
) WITHOUT ROWID, STRICT;
CREATE TABLE IF NOT EXISTS content_blobs (
    hash BLOB PRIMARY KEY NOT NULL CHECK(length(hash) = 32),
    raw_size INTEGER NOT NULL CHECK(raw_size > 0 AND raw_size <= 262144),
    codec INTEGER NOT NULL CHECK(codec IN (0, 1)),
    payload BLOB NOT NULL
) WITHOUT ROWID, STRICT;
CREATE TABLE IF NOT EXISTS content_objects (
    hash BLOB PRIMARY KEY NOT NULL CHECK(length(hash) = 32),
    raw_size INTEGER NOT NULL CHECK(raw_size >= 0 AND raw_size <= 536870912),
    inline_codec INTEGER CHECK(inline_codec IS NULL OR inline_codec IN (0, 1)),
    inline_payload BLOB,
    CHECK((raw_size <= 4096 AND inline_codec IS NOT NULL AND inline_payload IS NOT NULL)
       OR (raw_size > 4096 AND inline_codec IS NULL AND inline_payload IS NULL))
) WITHOUT ROWID, STRICT;
CREATE TABLE IF NOT EXISTS content_parts (
    content_hash BLOB NOT NULL REFERENCES content_objects(hash) ON DELETE CASCADE
        CHECK(length(content_hash) = 32),
    ordinal INTEGER NOT NULL CHECK(ordinal >= 0),
    byte_offset INTEGER NOT NULL CHECK(byte_offset >= 0),
    byte_length INTEGER NOT NULL CHECK(byte_length > 0 AND byte_length <= 262144),
    blob_hash BLOB NOT NULL REFERENCES content_blobs(hash) ON DELETE RESTRICT
        CHECK(length(blob_hash) = 32),
    PRIMARY KEY(content_hash, ordinal),
    UNIQUE(content_hash, byte_offset)
) WITHOUT ROWID, STRICT;
CREATE INDEX IF NOT EXISTS content_parts_by_blob
    ON content_parts(blob_hash);
CREATE TABLE IF NOT EXISTS content_gc_candidates (
    kind TEXT NOT NULL CHECK(kind IN ('object', 'blob')),
    hash BLOB NOT NULL CHECK(length(hash) = 32),
    PRIMARY KEY(kind, hash)
) WITHOUT ROWID, STRICT;

CREATE TRIGGER IF NOT EXISTS gc_checkpoint_files_delete AFTER DELETE ON checkpoint_files BEGIN
    INSERT OR IGNORE INTO content_gc_candidates(kind, hash) VALUES ('object', OLD.content_hash);
END;
CREATE TRIGGER IF NOT EXISTS gc_checkpoint_files_update AFTER UPDATE OF content_hash ON checkpoint_files
WHEN OLD.content_hash IS NOT NEW.content_hash BEGIN
    INSERT OR IGNORE INTO content_gc_candidates(kind, hash) VALUES ('object', OLD.content_hash);
END;
CREATE TRIGGER IF NOT EXISTS gc_run_messages_delete AFTER DELETE ON run_messages
WHEN OLD.content_hash IS NOT NULL BEGIN
    INSERT OR IGNORE INTO content_gc_candidates(kind, hash) VALUES ('object', OLD.content_hash);
END;
CREATE TRIGGER IF NOT EXISTS gc_run_messages_update AFTER UPDATE OF content_hash ON run_messages
WHEN OLD.content_hash IS NOT NULL AND OLD.content_hash IS NOT NEW.content_hash BEGIN
    INSERT OR IGNORE INTO content_gc_candidates(kind, hash) VALUES ('object', OLD.content_hash);
END;
CREATE TRIGGER IF NOT EXISTS gc_context_checkpoints_delete AFTER DELETE ON run_context_checkpoints BEGIN
    INSERT OR IGNORE INTO content_gc_candidates(kind, hash) VALUES ('object', OLD.summary_hash);
END;
CREATE TRIGGER IF NOT EXISTS gc_context_checkpoints_update AFTER UPDATE OF summary_hash ON run_context_checkpoints
WHEN OLD.summary_hash IS NOT NEW.summary_hash BEGIN
    INSERT OR IGNORE INTO content_gc_candidates(kind, hash) VALUES ('object', OLD.summary_hash);
END;
CREATE TRIGGER IF NOT EXISTS gc_message_fragments_delete AFTER DELETE ON run_message_fragments BEGIN
    INSERT OR IGNORE INTO content_gc_candidates(kind, hash) VALUES ('object', OLD.content_hash);
END;
CREATE TRIGGER IF NOT EXISTS gc_message_fragments_update AFTER UPDATE OF content_hash ON run_message_fragments
WHEN OLD.content_hash IS NOT NEW.content_hash BEGIN
    INSERT OR IGNORE INTO content_gc_candidates(kind, hash) VALUES ('object', OLD.content_hash);
END;
CREATE TRIGGER IF NOT EXISTS gc_activities_delete AFTER DELETE ON run_activities BEGIN
    INSERT OR IGNORE INTO content_gc_candidates(kind, hash) VALUES ('object', OLD.data_hash);
END;
CREATE TRIGGER IF NOT EXISTS gc_activities_update AFTER UPDATE OF data_hash ON run_activities
WHEN OLD.data_hash IS NOT NEW.data_hash BEGIN
    INSERT OR IGNORE INTO content_gc_candidates(kind, hash) VALUES ('object', OLD.data_hash);
END;
CREATE TRIGGER IF NOT EXISTS gc_tool_calls_delete AFTER DELETE ON run_tool_calls BEGIN
    INSERT OR IGNORE INTO content_gc_candidates(kind, hash) VALUES ('object', OLD.arguments_hash);
END;
CREATE TRIGGER IF NOT EXISTS gc_tool_calls_update AFTER UPDATE OF arguments_hash ON run_tool_calls
WHEN OLD.arguments_hash IS NOT NEW.arguments_hash BEGIN
    INSERT OR IGNORE INTO content_gc_candidates(kind, hash) VALUES ('object', OLD.arguments_hash);
END;
CREATE TRIGGER IF NOT EXISTS gc_tool_attempts_delete AFTER DELETE ON run_tool_attempts
WHEN OLD.result_hash IS NOT NULL BEGIN
    INSERT OR IGNORE INTO content_gc_candidates(kind, hash) VALUES ('object', OLD.result_hash);
END;
CREATE TRIGGER IF NOT EXISTS gc_tool_attempts_update AFTER UPDATE OF result_hash ON run_tool_attempts
WHEN OLD.result_hash IS NOT NULL AND OLD.result_hash IS NOT NEW.result_hash BEGIN
    INSERT OR IGNORE INTO content_gc_candidates(kind, hash) VALUES ('object', OLD.result_hash);
END;
CREATE TRIGGER IF NOT EXISTS gc_message_tool_calls_delete AFTER DELETE ON run_message_tool_calls BEGIN
    INSERT OR IGNORE INTO content_gc_candidates(kind, hash) VALUES ('object', OLD.arguments_hash);
END;
CREATE TRIGGER IF NOT EXISTS gc_message_tool_calls_update
AFTER UPDATE OF arguments_hash ON run_message_tool_calls
WHEN OLD.arguments_hash IS NOT NEW.arguments_hash BEGIN
    INSERT OR IGNORE INTO content_gc_candidates(kind, hash) VALUES ('object', OLD.arguments_hash);
END;
CREATE TRIGGER IF NOT EXISTS gc_runtime_configurations_delete AFTER DELETE ON runtime_configurations BEGIN
    INSERT OR IGNORE INTO content_gc_candidates(kind, hash)
        SELECT 'object', OLD.system_instructions_hash WHERE OLD.system_instructions_hash IS NOT NULL;
    INSERT OR IGNORE INTO content_gc_candidates(kind, hash)
        SELECT 'object', OLD.repository_instructions_hash WHERE OLD.repository_instructions_hash IS NOT NULL;
END;
CREATE TRIGGER IF NOT EXISTS gc_run_runtime_config_delete AFTER DELETE ON run_runtime_config BEGIN
    DELETE FROM runtime_configurations
    WHERE configuration_hash=OLD.configuration_hash
      AND NOT EXISTS (
          SELECT 1 FROM run_runtime_config
          WHERE run_runtime_config.configuration_hash=OLD.configuration_hash
      );
END;
CREATE TRIGGER IF NOT EXISTS gc_run_runtime_config_update
AFTER UPDATE OF configuration_hash ON run_runtime_config
WHEN OLD.configuration_hash IS NOT NEW.configuration_hash BEGIN
    DELETE FROM runtime_configurations
    WHERE configuration_hash=OLD.configuration_hash
      AND NOT EXISTS (
          SELECT 1 FROM run_runtime_config
          WHERE run_runtime_config.configuration_hash=OLD.configuration_hash
      );
END;
CREATE TRIGGER IF NOT EXISTS gc_runtime_configurations_update
AFTER UPDATE OF system_instructions_hash, repository_instructions_hash ON runtime_configurations BEGIN
    INSERT OR IGNORE INTO content_gc_candidates(kind, hash)
        SELECT 'object', OLD.system_instructions_hash
        WHERE OLD.system_instructions_hash IS NOT NULL
          AND OLD.system_instructions_hash IS NOT NEW.system_instructions_hash;
    INSERT OR IGNORE INTO content_gc_candidates(kind, hash)
        SELECT 'object', OLD.repository_instructions_hash
        WHERE OLD.repository_instructions_hash IS NOT NULL
          AND OLD.repository_instructions_hash IS NOT NEW.repository_instructions_hash;
END;
CREATE TRIGGER IF NOT EXISTS gc_filesystem_edits_delete AFTER DELETE ON filesystem_edits
WHEN OLD.before_hash IS NOT NULL BEGIN
    INSERT OR IGNORE INTO content_gc_candidates(kind, hash) VALUES ('object', OLD.before_hash);
END;
CREATE TRIGGER IF NOT EXISTS gc_filesystem_edits_update AFTER UPDATE OF before_hash ON filesystem_edits
WHEN OLD.before_hash IS NOT NULL AND OLD.before_hash IS NOT NEW.before_hash BEGIN
    INSERT OR IGNORE INTO content_gc_candidates(kind, hash) VALUES ('object', OLD.before_hash);
END;
CREATE TRIGGER IF NOT EXISTS gc_content_parts_delete AFTER DELETE ON content_parts BEGIN
    INSERT OR IGNORE INTO content_gc_candidates(kind, hash) VALUES ('object', OLD.content_hash);
    INSERT OR IGNORE INTO content_gc_candidates(kind, hash) VALUES ('blob', OLD.blob_hash);
END;
CREATE TRIGGER IF NOT EXISTS gc_content_parts_update AFTER UPDATE ON content_parts BEGIN
    INSERT OR IGNORE INTO content_gc_candidates(kind, hash)
        SELECT 'object', OLD.content_hash WHERE OLD.content_hash IS NOT NEW.content_hash;
    INSERT OR IGNORE INTO content_gc_candidates(kind, hash)
        SELECT 'blob', OLD.blob_hash WHERE OLD.blob_hash IS NOT NEW.blob_hash;
END;
";

fn decode_content_blob(connection: &Connection, hash: &[u8]) -> Result<Vec<u8>> {
    let (raw_size, codec, payload): (i64, i64, Vec<u8>) = connection
        .query_row(
            "SELECT raw_size, codec, payload FROM content_blobs WHERE hash = ?1",
            [hash],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .map_err(|error| {
            persistence_error(format!("could not read state content part: {error}"), true)
        })?;
    if raw_size <= 0 {
        return Err(LoomError::new(
            ErrorCode::MalformedPayload,
            "persisted content part has an invalid size",
            false,
        ));
    }
    decode_content_payload(
        hash,
        raw_size,
        codec,
        payload,
        CONTENT_PART_BYTES,
        "content part",
    )
}

fn decode_content_payload(
    hash: &[u8],
    raw_size: i64,
    codec: i64,
    payload: Vec<u8>,
    max_size: usize,
    kind: &str,
) -> Result<Vec<u8>> {
    if raw_size < 0 || raw_size > max_size as i64 {
        return Err(LoomError::new(
            ErrorCode::MalformedPayload,
            format!("persisted {kind} has an invalid size"),
            false,
        ));
    }
    let bytes = match codec {
        0 => payload,
        1 => {
            let mut decoded = Vec::with_capacity(raw_size as usize);
            ZlibDecoder::new(payload.as_slice())
                .take((raw_size as u64).saturating_add(1))
                .read_to_end(&mut decoded)
                .map_err(|error| {
                    LoomError::new(
                        ErrorCode::MalformedPayload,
                        format!("persisted {kind} is malformed: {error}"),
                        false,
                    )
                })?;
            decoded
        }
        _ => {
            return Err(LoomError::new(
                ErrorCode::MalformedPayload,
                format!("persisted {kind} uses an unsupported codec"),
                false,
            ));
        }
    };
    if bytes.len() as i64 != raw_size || Sha256::digest(&bytes).as_slice() != hash {
        return Err(LoomError::new(
            ErrorCode::MalformedPayload,
            format!("persisted {kind} failed its length or hash check"),
            false,
        ));
    }
    Ok(bytes)
}

fn load_content_range(
    connection: &Connection,
    hash: &[u8],
    offset: usize,
    requested_len: usize,
) -> Result<Vec<u8>> {
    let (raw_size, inline_codec, inline_payload): (i64, Option<i64>, Option<Vec<u8>>) = connection
        .query_row(
            "SELECT raw_size, inline_codec, inline_payload FROM content_objects WHERE hash = ?1",
            [hash],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .map_err(|error| {
            persistence_error(
                format!("could not read state content metadata: {error}"),
                true,
            )
        })?;
    if raw_size < 0 || raw_size > MAX_CONTENT_BYTES as i64 {
        return Err(LoomError::new(
            ErrorCode::MalformedPayload,
            "persisted state content exceeds the maximum supported size",
            false,
        ));
    }
    let start = offset.min(raw_size as usize);
    let end = start.saturating_add(requested_len).min(raw_size as usize);
    if start == end {
        return Ok(Vec::new());
    };
    if let (Some(codec), Some(payload)) = (inline_codec, inline_payload) {
        let bytes = decode_content_payload(
            hash,
            raw_size,
            codec,
            payload,
            INLINE_CONTENT_BYTES,
            "inline content",
        )?;
        return Ok(bytes[start..end].to_vec());
    }
    let mut statement = connection
        .prepare(
            "SELECT byte_offset, byte_length, blob_hash FROM content_parts
             WHERE content_hash=?1 AND byte_offset < ?2
               AND byte_offset + byte_length > ?3
             ORDER BY byte_offset",
        )
        .map_err(|error| {
            persistence_error(format!("could not prepare content range: {error}"), true)
        })?;
    let parts = statement
        .query_map(params![hash, end as i64, start as i64], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, Vec<u8>>(2)?,
            ))
        })
        .map_err(|error| {
            persistence_error(format!("could not read content range: {error}"), true)
        })?;
    let mut output = Vec::with_capacity(end - start);
    let mut cursor = start;
    for part in parts {
        let (part_offset, part_length, blob_hash) = part.map_err(|error| {
            persistence_error(format!("could not read content part: {error}"), true)
        })?;
        let part_offset = usize::try_from(part_offset).map_err(|_| {
            LoomError::new(
                ErrorCode::MalformedPayload,
                "persisted content part has an invalid offset",
                false,
            )
        })?;
        let part_length = usize::try_from(part_length).map_err(|_| {
            LoomError::new(
                ErrorCode::MalformedPayload,
                "persisted content part has an invalid length",
                false,
            )
        })?;
        let part_end = part_offset.checked_add(part_length).ok_or_else(|| {
            LoomError::new(
                ErrorCode::MalformedPayload,
                "persisted content part range overflowed",
                false,
            )
        })?;
        let slice_start = start.max(part_offset);
        let slice_end = end.min(part_end);
        if slice_start != cursor || slice_start >= slice_end {
            return Err(LoomError::new(
                ErrorCode::MalformedPayload,
                "persisted content parts are not contiguous",
                false,
            ));
        }
        let part_bytes = decode_content_blob(connection, &blob_hash)?;
        if part_bytes.len() != part_length {
            return Err(LoomError::new(
                ErrorCode::MalformedPayload,
                "persisted content part length does not match its range",
                false,
            ));
        }
        output.extend_from_slice(&part_bytes[slice_start - part_offset..slice_end - part_offset]);
        cursor = slice_end;
    }
    if cursor != end {
        return Err(LoomError::new(
            ErrorCode::MalformedPayload,
            "persisted content range is incomplete",
            false,
        ));
    }
    Ok(output)
}

fn decode_content(connection: &Connection, hash: &[u8]) -> Result<String> {
    let raw_size: i64 = connection
        .query_row(
            "SELECT raw_size FROM content_objects WHERE hash = ?1",
            [hash],
            |row| row.get(0),
        )
        .map_err(|error| {
            persistence_error(
                format!("could not read state content metadata: {error}"),
                true,
            )
        })?;
    let bytes = load_content_range(connection, hash, 0, raw_size.max(0) as usize)?;
    if bytes.len() as i64 != raw_size || Sha256::digest(&bytes).as_slice() != hash {
        return Err(LoomError::new(
            ErrorCode::MalformedPayload,
            "persisted state content failed its length or hash check",
            false,
        ));
    }
    String::from_utf8(bytes).map_err(|error| {
        LoomError::new(
            ErrorCode::MalformedPayload,
            format!("persisted state text is not UTF-8: {error}"),
            false,
        )
    })
}

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
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DurableRunContextCheckpoint {
    pub session_id: AgentSessionId,
    pub summary: ContextSummary,
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

impl FilePersistence {
    pub fn new(path: impl Into<PathBuf>) -> Result<Self> {
        Self::open(path)
    }

    pub fn open(path: impl Into<PathBuf>) -> Result<Self> {
        let path = path.into();
        if path.as_os_str().is_empty() {
            return Err(LoomError::invalid_request(
                "persistence path must not be empty",
            ));
        }

        Ok(Self {
            path,
            connection: Arc::new(Mutex::new(None)),
            owner_lock: Arc::new(Mutex::new(None)),
        })
    }

    /// Opens the store as the exclusive writer for a backend process. The lock
    /// is advisory and remains held by this handle and its clones until they
    /// are all dropped. Read-only/diagnostic handles may still use `open`.
    pub fn open_exclusive_writer(path: impl Into<PathBuf>) -> Result<Self> {
        let path = path.into();
        if path.as_os_str().is_empty() {
            return Err(LoomError::invalid_request(
                "persistence path must not be empty",
            ));
        }
        let absolute_path = if path.exists() {
            path.canonicalize().map_err(|error| {
                persistence_error(format!("could not resolve persistence path: {error}"), true)
            })?
        } else {
            if let Some(parent) = path
                .parent()
                .filter(|parent| !parent.as_os_str().is_empty())
            {
                fs::create_dir_all(parent).map_err(|error| {
                    persistence_error(
                        format!(
                            "could not create persistence directory '{}': {error}",
                            parent.display()
                        ),
                        true,
                    )
                })?;
            }
            let parent = path.parent().unwrap_or_else(|| Path::new("."));
            let parent = parent.canonicalize().map_err(|error| {
                persistence_error(
                    format!("could not resolve persistence directory: {error}"),
                    true,
                )
            })?;
            parent.join(path.file_name().ok_or_else(|| {
                LoomError::invalid_request("persistence path must name a database file")
            })?)
        };
        let mut lock_path = absolute_path.as_os_str().to_os_string();
        lock_path.push(".loom-owner.lock");
        let lock_file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(PathBuf::from(lock_path))
            .map_err(|error| {
                persistence_error(
                    format!("could not open persistence owner lock: {error}"),
                    true,
                )
            })?;
        match lock_file.try_lock() {
            Ok(()) => {}
            Err(std::fs::TryLockError::WouldBlock) => {
                return Err(LoomError::conflict(format!(
                    "persistence database '{}' is already owned by another backend",
                    absolute_path.display()
                )));
            }
            Err(std::fs::TryLockError::Error(error)) => {
                return Err(persistence_error(
                    format!("could not acquire persistence owner lock: {error}"),
                    true,
                ));
            }
        }
        let persistence = Self::open(path)?;
        *persistence.owner_lock.lock().map_err(|_| {
            LoomError::new(
                ErrorCode::Persistence,
                "persistence owner lock state was poisoned",
                true,
            )
        })? = Some(lock_file);
        Ok(persistence)
    }

    /// Releases exclusive writer ownership after the backend has stopped and
    /// joined every worker. Cloned store handles share this ownership slot.
    pub fn release_exclusive_writer(&self) -> Result<()> {
        self.owner_lock
            .lock()
            .map_err(|_| {
                LoomError::new(
                    ErrorCode::Persistence,
                    "persistence owner lock state was poisoned",
                    true,
                )
            })?
            .take();
        Ok(())
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn exists(&self) -> bool {
        self.path.is_file()
    }

    /// Persists the typed session catalog without rewriting unrelated state.
    pub fn save_state_with_sessions(&self, sessions: &SessionManagerState) -> Result<()> {
        self.save_state(DurableStateWrite {
            sessions,
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
            run_activities: None,
            filesystem_records: None,
            feed: None,
        })
    }

    /// Persists the typed catalogs and reconnect feed atomically.
    pub fn save_state_with_catalogs_and_feed(
        &self,
        sessions: &SessionManagerState,
        workspaces: &WorkspaceManagerState,
        feed: Option<&DurableFeedState>,
    ) -> Result<()> {
        self.save_state(DurableStateWrite {
            sessions,
            workspaces: Some(workspaces),
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
            run_activities: None,
            filesystem_records: None,
            feed,
        })
    }

    /// Persists the typed session catalog and reconnect feed atomically.
    pub fn save_state_with_sessions_and_feed(
        &self,
        sessions: &SessionManagerState,
        feed: Option<&DurableFeedState>,
    ) -> Result<()> {
        self.save_state(DurableStateWrite {
            sessions,
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
            run_activities: None,
            filesystem_records: None,
            feed,
        })
    }

    /// Loads the indexed session catalog and its lifecycle sequence cursor.
    pub fn load_sessions(&self) -> Result<Option<SessionManagerState>> {
        if !self.path.exists() {
            return Ok(None);
        }
        let connection = self.connection()?;
        let next_sequence = connection
            .query_row(
                "SELECT next_sequence FROM session_store_meta WHERE singleton = 1",
                [],
                |row| row.get::<_, i64>(0),
            )
            .optional()
            .map_err(|error| {
                persistence_error(format!("could not read session catalog: {error}"), true)
            })?;
        let Some(next_sequence) = next_sequence else {
            return Ok(None);
        };
        let mut statement = connection
            .prepare(
                "SELECT id, workspace_id, name, state, created_at, updated_at
                 FROM sessions ORDER BY id",
            )
            .map_err(|error| {
                persistence_error(format!("could not prepare session catalog: {error}"), true)
            })?;
        let rows = statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, Vec<u8>>(0)?,
                    row.get::<_, Vec<u8>>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, i64>(4)?,
                    row.get::<_, i64>(5)?,
                ))
            })
            .map_err(|error| {
                persistence_error(format!("could not read session catalog: {error}"), true)
            })?;
        let mut sessions = BTreeMap::new();
        for row in rows {
            let (id, workspace_id, name, state, created_at, updated_at) = row.map_err(|error| {
                persistence_error(format!("could not read session catalog: {error}"), true)
            })?;
            let id = AgentSessionId::from_uuid(decode_uuid(&id, "session id")?);
            let workspace_id = WorkspaceId::from_uuid(decode_uuid(&workspace_id, "workspace id")?);
            let snapshot = AgentSessionSnapshot {
                id,
                workspace_id,
                name,
                state: parse_session_state(&state)?,
                created_at: decode_timestamp(created_at)?,
                updated_at: decode_timestamp(updated_at)?,
            };
            sessions.insert(id, snapshot);
        }
        Ok(Some(SessionManagerState {
            sessions,
            next_sequence: EventSequence::new(u64::try_from(next_sequence).map_err(|_| {
                LoomError::new(
                    ErrorCode::MalformedPayload,
                    "persisted session sequence is negative",
                    false,
                )
            })?),
        }))
    }

    /// Loads the small, typed workspace catalog.
    pub fn load_workspaces(&self) -> Result<Option<WorkspaceManagerState>> {
        if !self.path.exists() {
            return Ok(None);
        }
        let connection = self.connection()?;
        let mut statement = connection
            .prepare("SELECT id, name, created_at, updated_at FROM workspaces ORDER BY id")
            .map_err(|error| {
                persistence_error(
                    format!("could not prepare workspace catalog: {error}"),
                    true,
                )
            })?;
        let rows = statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, Vec<u8>>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, i64>(3)?,
                ))
            })
            .map_err(|error| {
                persistence_error(format!("could not read workspace catalog: {error}"), true)
            })?;
        let mut workspaces = BTreeMap::new();
        for row in rows {
            let (id, name, created_at, updated_at) = row.map_err(|error| {
                persistence_error(format!("could not read workspace catalog: {error}"), true)
            })?;
            let id = WorkspaceId::from_uuid(decode_uuid(&id, "workspace id")?);
            let workspace = WorkspaceRecord {
                id,
                name,
                created_at: decode_timestamp(created_at)?,
                updated_at: decode_timestamp(updated_at)?,
            };
            workspaces.insert(id, workspace);
        }
        Ok(Some(WorkspaceManagerState { workspaces }))
    }

    /// Loads policy and auto-approval settings by session key.
    pub fn load_session_settings(&self) -> Result<DurableSessionSettings> {
        if !self.path.exists() {
            return Ok(DurableSessionSettings::default());
        }
        let connection = self.connection()?;
        let mut statement = connection
            .prepare(
                "SELECT session_id, policy_read, policy_write, policy_command, policy_network,
                             policy_destructive, auto_approve_actions
                      FROM session_settings ORDER BY session_id",
            )
            .map_err(|error| {
                persistence_error(format!("could not prepare session settings: {error}"), true)
            })?;
        let rows = statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, Vec<u8>>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, String>(5)?,
                    row.get::<_, i64>(6)?,
                ))
            })
            .map_err(|error| {
                persistence_error(format!("could not read session settings: {error}"), true)
            })?;
        let mut settings = DurableSessionSettings::default();
        for row in rows {
            let (id, read, write, command, network, destructive, auto_approve) =
                row.map_err(|error| {
                    persistence_error(format!("could not read session settings: {error}"), true)
                })?;
            let id = AgentSessionId::from_uuid(decode_uuid(&id, "session id")?);
            let policy = ApprovalPolicy {
                read: decode_policy_decision(&read, "read")?,
                write: decode_policy_decision(&write, "write")?,
                command: decode_policy_decision(&command, "command")?,
                network: decode_policy_decision(&network, "network")?,
                destructive: decode_policy_decision(&destructive, "destructive")?,
            };
            let auto_approve = match auto_approve {
                0 => false,
                1 => true,
                _ => {
                    return Err(LoomError::new(
                        ErrorCode::MalformedPayload,
                        "persisted auto-approval flag is invalid",
                        false,
                    ));
                }
            };
            settings.approval_policies.insert(id, policy);
            settings.auto_approve_actions.insert(id, auto_approve);
        }
        Ok(settings)
    }

    /// Loads bounded workspace configuration records by workspace key.
    pub fn load_workspace_configs(&self) -> Result<BTreeMap<WorkspaceId, WorkspaceConfig>> {
        if !self.path.exists() {
            return Ok(BTreeMap::new());
        }
        let connection = self.connection()?;
        let mut statement = connection
            .prepare("SELECT workspace_id, revision, config FROM workspace_configs ORDER BY workspace_id")
            .map_err(|error| {
                persistence_error(format!("could not prepare workspace configs: {error}"), true)
            })?;
        let rows = statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, Vec<u8>>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, String>(2)?,
                ))
            })
            .map_err(|error| {
                persistence_error(format!("could not read workspace configs: {error}"), true)
            })?;
        let mut configs = BTreeMap::new();
        for row in rows {
            let (id, revision, config) = row.map_err(|error| {
                persistence_error(format!("could not read workspace configs: {error}"), true)
            })?;
            let id = WorkspaceId::from_uuid(decode_uuid(&id, "workspace id")?);
            let revision = u64::try_from(revision).map_err(|_| {
                LoomError::new(
                    ErrorCode::MalformedPayload,
                    "persisted workspace configuration revision is negative",
                    false,
                )
            })?;
            let config: WorkspaceConfig = serde_json::from_str(&config).map_err(|error| {
                LoomError::new(
                    ErrorCode::MalformedPayload,
                    format!("persisted workspace configuration is malformed: {error}"),
                    false,
                )
            })?;
            if config.revision != revision {
                return Err(LoomError::new(
                    ErrorCode::MalformedPayload,
                    "workspace configuration revision does not match its index",
                    false,
                ));
            }
            configs.insert(id, config);
        }
        Ok(configs)
    }

    pub fn load_provider_configs(&self) -> Result<Vec<ProviderConfig>> {
        if !self.path.exists() {
            return Ok(Vec::new());
        }
        let connection = self.connection()?;
        let mut statement = connection
            .prepare("SELECT provider_id, config FROM provider_configs ORDER BY provider_id")
            .map_err(|error| {
                persistence_error(format!("could not prepare provider configs: {error}"), true)
            })?;
        let rows = statement
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .map_err(|error| {
                persistence_error(format!("could not read provider configs: {error}"), true)
            })?;
        let mut configs = Vec::new();
        for row in rows {
            let (provider_id, config) = row.map_err(|error| {
                persistence_error(format!("could not read provider configs: {error}"), true)
            })?;
            let config: ProviderConfig = serde_json::from_str(&config).map_err(|error| {
                LoomError::new(
                    ErrorCode::MalformedPayload,
                    format!("persisted provider configuration is malformed: {error}"),
                    false,
                )
            })?;
            if config.id.as_str() != provider_id {
                return Err(LoomError::new(
                    ErrorCode::MalformedPayload,
                    "provider configuration id does not match its index",
                    false,
                ));
            }
            configs.push(config);
        }
        Ok(configs)
    }

    pub fn load_provider_health(&self) -> Result<BTreeMap<ProviderId, ProviderHealth>> {
        if !self.path.exists() {
            return Ok(BTreeMap::new());
        }
        let connection = self.connection()?;
        let mut statement = connection
            .prepare("SELECT provider_id, health FROM provider_health ORDER BY provider_id")
            .map_err(|error| {
                persistence_error(format!("could not prepare provider health: {error}"), true)
            })?;
        let rows = statement
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .map_err(|error| {
                persistence_error(format!("could not read provider health: {error}"), true)
            })?;
        let mut health = BTreeMap::new();
        for row in rows {
            let (provider_id, state) = row.map_err(|error| {
                persistence_error(format!("could not read provider health: {error}"), true)
            })?;
            let state = serde_json::from_str(&state).map_err(|error| {
                LoomError::new(
                    ErrorCode::MalformedPayload,
                    format!("persisted provider health is malformed: {error}"),
                    false,
                )
            })?;
            health.insert(ProviderId::new(provider_id), state);
        }
        Ok(health)
    }

    pub fn load_provider_usage(&self) -> Result<UsageLedger> {
        if !self.path.exists() {
            return Ok(UsageLedger::default());
        }
        let connection = self.connection()?;
        let mut statement = connection
            .prepare(
                "SELECT provider_id, model_id, requests, input_tokens, output_tokens,
                        cached_input_tokens, cost_micros
                 FROM provider_usage_totals ORDER BY provider_id, model_id",
            )
            .map_err(|error| {
                persistence_error(
                    format!("could not prepare provider usage totals: {error}"),
                    true,
                )
            })?;
        let rows = statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, i64>(4)?,
                    row.get::<_, i64>(5)?,
                    row.get::<_, i64>(6)?,
                ))
            })
            .map_err(|error| {
                persistence_error(
                    format!("could not read provider usage totals: {error}"),
                    true,
                )
            })?;
        let mut ledger = UsageLedger::default();
        for row in rows {
            let (provider_id, model_id, requests, input, output, cached, cost) =
                row.map_err(|error| {
                    persistence_error(
                        format!("could not read provider usage totals: {error}"),
                        true,
                    )
                })?;
            ledger.aggregates.insert(
                ProviderUsageKey {
                    provider: ProviderId::new(provider_id),
                    model: ModelId::new(model_id),
                },
                ProviderUsageSummary {
                    requests: decode_counter(requests, "provider request count")?,
                    input_tokens: decode_counter(input, "provider input token count")?,
                    output_tokens: decode_counter(output, "provider output token count")?,
                    cached_input_tokens: decode_counter(cached, "cached input token count")?,
                    cost_micros: decode_counter(cost, "provider usage cost")?,
                },
            );
        }
        Ok(ledger)
    }

    pub fn load_idempotency_records(
        &self,
    ) -> Result<BTreeMap<RequestId, DurableIdempotencyRecord>> {
        if !self.path.exists() {
            return Ok(BTreeMap::new());
        }
        let connection = self.connection()?;
        let mut statement = connection
            .prepare(
                "SELECT request_id, created_at, expires_at, request_hash, request, response
                 FROM idempotency_records ORDER BY created_at, request_id",
            )
            .map_err(|error| {
                persistence_error(
                    format!("could not prepare idempotency records: {error}"),
                    true,
                )
            })?;
        let rows = statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, Vec<u8>>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, Option<i64>>(2)?,
                    row.get::<_, Vec<u8>>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, String>(5)?,
                ))
            })
            .map_err(|error| {
                persistence_error(format!("could not read idempotency records: {error}"), true)
            })?;
        let mut records = BTreeMap::new();
        for row in rows {
            let (id, created_at, expires_at, request_hash, request, response) =
                row.map_err(|error| {
                    persistence_error(format!("could not read idempotency records: {error}"), true)
                })?;
            let id = RequestId::from_uuid(decode_uuid(&id, "request id")?);
            let actual_hash = Sha256::digest(request.as_bytes());
            if actual_hash.as_slice() != request_hash {
                return Err(LoomError::new(
                    ErrorCode::MalformedPayload,
                    "persisted idempotency request failed its hash check",
                    false,
                ));
            }
            let request = serde_json::from_str(&request).map_err(|error| {
                LoomError::new(
                    ErrorCode::MalformedPayload,
                    format!("persisted idempotency request is malformed: {error}"),
                    false,
                )
            })?;
            let response = serde_json::from_str(&response).map_err(|error| {
                LoomError::new(
                    ErrorCode::MalformedPayload,
                    format!("persisted idempotency response is malformed: {error}"),
                    false,
                )
            })?;
            records.insert(
                id,
                DurableIdempotencyRecord {
                    created_at: decode_timestamp(created_at)?,
                    expires_at: expires_at.map(decode_timestamp).transpose()?,
                    request,
                    response,
                },
            );
        }
        Ok(records)
    }

    /// Removes response-cache rows whose explicit retry horizon has ended.
    pub fn prune_expired_idempotency_records(&self, now: Timestamp) -> Result<usize> {
        let connection = self.connection_for_write()?;
        connection
            .execute(
                "DELETE FROM idempotency_records
                 WHERE expires_at IS NOT NULL AND expires_at <= ?1",
                [encode_timestamp(now)?],
            )
            .map_err(|error| {
                persistence_error(
                    format!("could not prune expired idempotency records: {error}"),
                    true,
                )
            })
    }

    /// Loads all indexed run summaries without reading runtime details or transcripts.
    pub fn load_run_summaries(&self) -> Result<BTreeMap<RunId, DurableRunSummary>> {
        self.load_run_summaries_matching("", [], "")
    }

    /// Loads only resumable run summaries for startup recovery.
    pub fn load_active_run_summaries(&self) -> Result<BTreeMap<RunId, DurableRunSummary>> {
        self.load_run_summaries_matching(
            "WHERE state NOT IN ('completed', 'failed', 'cancelled')",
            [],
            "",
        )
    }

    /// Loads summaries belonging to one session, on demand.
    pub fn load_run_summaries_for_session(
        &self,
        session_id: AgentSessionId,
    ) -> Result<BTreeMap<RunId, DurableRunSummary>> {
        self.load_run_summaries_matching(
            "WHERE session_id=?1",
            [session_id.as_uuid().as_bytes().as_slice()],
            "",
        )
    }

    /// Loads one run summary without scanning unrelated run history.
    pub fn load_run_summary(&self, run_id: RunId) -> Result<Option<DurableRunSummary>> {
        Ok(self
            .load_run_summaries_matching(
                "WHERE run_id=?1",
                [run_id.as_uuid().as_bytes().as_slice()],
                "LIMIT 1",
            )?
            .into_values()
            .next())
    }

    /// Loads one run's typed context checkpoint without reading its runtime payload.
    pub fn load_run_context_checkpoint(
        &self,
        run_id: RunId,
    ) -> Result<Option<DurableRunContextCheckpoint>> {
        if !self.path.exists() {
            return Ok(None);
        }
        let connection = self.connection()?;
        Self::load_run_context_checkpoint_on(&connection, run_id)
    }

    fn load_run_context_checkpoint_on(
        connection: &Connection,
        run_id: RunId,
    ) -> Result<Option<DurableRunContextCheckpoint>> {
        let row = connection
            .query_row(
                "SELECT session_id, summary_hash, source_message_count,
                        projection_version, source_digest, created_at
                 FROM run_context_checkpoints WHERE run_id=?1",
                [run_id.as_uuid().as_bytes().as_slice()],
                |row| {
                    Ok((
                        row.get::<_, Vec<u8>>(0)?,
                        row.get::<_, Vec<u8>>(1)?,
                        row.get::<_, i64>(2)?,
                        row.get::<_, i64>(3)?,
                        row.get::<_, String>(4)?,
                        row.get::<_, i64>(5)?,
                    ))
                },
            )
            .optional()
            .map_err(|error| {
                persistence_error(
                    format!("could not read run context checkpoint: {error}"),
                    true,
                )
            })?;
        let Some((session_id, summary_hash, source_count, version, source_digest, created_at)) =
            row
        else {
            return Ok(None);
        };
        let source_count = usize::try_from(decode_counter(source_count, "context message count")?)
            .map_err(|_| {
                LoomError::new(
                    ErrorCode::MalformedPayload,
                    "persisted context message count is out of range",
                    false,
                )
            })?;
        let projection_version =
            u32::try_from(decode_counter(version, "context projection version")?).map_err(
                |_| {
                    LoomError::new(
                        ErrorCode::MalformedPayload,
                        "persisted context projection version is out of range",
                        false,
                    )
                },
            )?;
        let summary = String::from_utf8(decode_content(connection, &summary_hash)?.into_bytes())
            .map_err(|error| {
                LoomError::new(
                    ErrorCode::MalformedPayload,
                    format!("persisted context summary is not UTF-8: {error}"),
                    false,
                )
            })?;
        Ok(Some(DurableRunContextCheckpoint {
            session_id: AgentSessionId::from_uuid(decode_uuid(&session_id, "context session id")?),
            summary: ContextSummary {
                text: summary,
                source_message_count: source_count,
                projection_version,
                source_digest,
                created_at: decode_timestamp(created_at)?,
            },
        }))
    }

    /// Loads the latest summary for a session through its activity index.
    pub fn load_latest_run_summary_for_session(
        &self,
        session_id: AgentSessionId,
    ) -> Result<Option<DurableRunSummary>> {
        Ok(self
            .load_run_summaries_matching(
                "WHERE session_id=?1",
                [session_id.as_uuid().as_bytes().as_slice()],
                "LIMIT 1",
            )?
            .into_values()
            .next())
    }

    /// Reads the persisted latest-run bootstrap inputs and feed cursor from a
    /// single deferred SQLite read transaction. Live handles and the in-memory
    /// journal remain server-owned overlays and are not part of this snapshot.
    pub fn load_session_projection_read(
        &self,
        session_id: AgentSessionId,
    ) -> Result<DurableSessionProjectionRead> {
        self.load_session_projection_read_between(session_id, || Ok(()))
    }

    fn load_session_projection_read_between<F>(
        &self,
        session_id: AgentSessionId,
        between_reads: F,
    ) -> Result<DurableSessionProjectionRead>
    where
        F: FnOnce() -> Result<()>,
    {
        if !self.path.exists() {
            return Ok(DurableSessionProjectionRead {
                latest_run: None,
                runtime_config: None,
                execution_state: None,
                plan: AgentPlan { steps: Vec::new() },
                context_checkpoint: None,
                activities: Vec::new(),
                attempts: Vec::new(),
                interactions: Vec::new(),
                latest_sequence: None,
            });
        }
        let connection = self.connection()?;
        let transaction = connection.unchecked_transaction().map_err(|error| {
            persistence_error(
                format!("could not begin session projection read: {error}"),
                true,
            )
        })?;
        // The first SELECT establishes the deferred transaction's read snapshot.
        let latest_sequence = Self::load_feed_session_cursor_on(&transaction, session_id)?
            .map(|cursor| cursor.latest_sequence);
        between_reads()?;
        let latest_run = Self::load_run_summaries_matching_on(
            &transaction,
            "WHERE session_id=?1",
            [session_id.as_uuid().as_bytes().as_slice()],
            "LIMIT 1",
        )?
        .into_values()
        // The shared query applies ORDER BY updated_at DESC, run_id DESC before
        // LIMIT 1, so this is the same deterministic latest-run selection as
        // `load_latest_run_summary_for_session`.
        .next();
        let (
            runtime_config,
            execution_state,
            plan,
            context_checkpoint,
            activities,
            attempts,
            interactions,
        ) = if let Some(summary) = latest_run.as_ref() {
            (
                Self::load_run_runtime_config_on(&transaction, summary.snapshot.id)?,
                Self::load_run_execution_state_on(&transaction, summary.snapshot.id)?,
                Self::load_run_plan_on(&transaction, summary.snapshot.id)?,
                Self::load_run_context_checkpoint_on(&transaction, summary.snapshot.id)?,
                Self::load_run_activities_on(&transaction, summary.snapshot.id)?,
                Self::load_run_attempts_on(&transaction, summary.snapshot.id)?,
                Self::load_run_interactions_on(&transaction, summary.snapshot.id)?,
            )
        } else {
            (
                None,
                None,
                AgentPlan { steps: Vec::new() },
                None,
                Vec::new(),
                Vec::new(),
                Vec::new(),
            )
        };
        // These columns are part of the bootstrap projection and must be
        // present in the same snapshot as the run and cursor.
        transaction.commit().map_err(|error| {
            persistence_error(
                format!("could not finish session projection read: {error}"),
                true,
            )
        })?;
        Ok(DurableSessionProjectionRead {
            latest_run,
            runtime_config,
            execution_state,
            plan,
            context_checkpoint,
            activities,
            attempts,
            interactions,
            latest_sequence,
        })
    }

    /// Aggregates typed per-run usage in SQLite, optionally excluding runs whose
    /// live in-memory counters are newer than the persisted summary rows.
    pub fn load_session_usage(
        &self,
        session_id: AgentSessionId,
        excluded_runs: &BTreeSet<RunId>,
    ) -> Result<UsageSnapshot> {
        if !self.path.exists() {
            return Ok(UsageSnapshot::default());
        }
        let mut sql = "SELECT COALESCE(SUM(input_tokens), 0),
                              COALESCE(SUM(output_tokens), 0),
                              COALESCE(SUM(cached_input_tokens), 0),
                              COALESCE(SUM(tool_calls), 0),
                              COALESCE(SUM(cost_micros), 0),
                              COALESCE(MAX(elapsed_ms), 0)
                       FROM run_summaries WHERE session_id=?1"
            .to_owned();
        let mut values = vec![rusqlite::types::Value::Blob(
            session_id.as_uuid().as_bytes().to_vec(),
        )];
        if !excluded_runs.is_empty() {
            sql.push_str(" AND run_id NOT IN (");
            for (index, run_id) in excluded_runs.iter().enumerate() {
                if index > 0 {
                    sql.push(',');
                }
                sql.push('?');
                sql.push_str(&(index + 2).to_string());
                values.push(rusqlite::types::Value::Blob(
                    run_id.as_uuid().as_bytes().to_vec(),
                ));
            }
            sql.push(')');
        }
        let connection = self.connection()?;
        let counters = connection
            .query_row(&sql, rusqlite::params_from_iter(values), |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, i64>(4)?,
                    row.get::<_, i64>(5)?,
                ))
            })
            .map_err(|error| {
                persistence_error(
                    format!("could not aggregate session run usage: {error}"),
                    true,
                )
            })?;
        Ok(UsageSnapshot {
            input_tokens: decode_counter(counters.0, "input token total")?,
            output_tokens: decode_counter(counters.1, "output token total")?,
            cached_input_tokens: decode_counter(counters.2, "cached input token total")?,
            tool_calls: decode_counter(counters.3, "tool call total")?,
            cost_micros: decode_counter(counters.4, "cost total")?,
            elapsed_ms: decode_counter(counters.5, "elapsed time")?,
        })
    }

    /// Loads a run's ordered plan steps without decoding its runtime snapshot.
    pub fn load_run_plan(&self, run_id: RunId) -> Result<AgentPlan> {
        if !self.path.exists() {
            return Ok(AgentPlan { steps: Vec::new() });
        }
        let connection = self.connection()?;
        Self::load_run_plan_on(&connection, run_id)
    }

    fn load_run_plan_on(connection: &Connection, run_id: RunId) -> Result<AgentPlan> {
        let mut statement = connection
            .prepare(
                "SELECT step_id, description FROM run_plan_steps WHERE run_id=?1 ORDER BY ordinal",
            )
            .map_err(|error| {
                persistence_error(format!("could not prepare run plan: {error}"), true)
            })?;
        let rows = statement
            .query_map([run_id.as_uuid().as_bytes().as_slice()], |row| {
                Ok(AgentPlanStep {
                    id: row.get(0)?,
                    description: row.get(1)?,
                })
            })
            .map_err(|error| {
                persistence_error(format!("could not read run plan: {error}"), true)
            })?;
        let mut steps = Vec::new();
        for row in rows {
            steps.push(row.map_err(|error| {
                persistence_error(format!("could not read run plan: {error}"), true)
            })?);
        }
        Ok(AgentPlan { steps })
    }

    pub fn load_run_runtime_config(
        &self,
        run_id: RunId,
    ) -> Result<Option<DurableRunRuntimeConfig>> {
        if !self.path.exists() {
            return Ok(None);
        }
        let connection = self.connection()?;
        Self::load_run_runtime_config_on(&connection, run_id)
    }

    fn load_run_runtime_config_on(
        connection: &Connection,
        run_id: RunId,
    ) -> Result<Option<DurableRunRuntimeConfig>> {
        let row = connection
            .query_row(
                "SELECT system_instructions_hash, repository_instructions_hash,
                        policy_read, policy_write, policy_command, policy_network,
                        policy_destructive, max_duration_ms, max_input_tokens,
                        max_output_tokens, max_tool_calls, max_cost_micros,
                        context_window, context_max_input_tokens,
                        context_reserved_output_tokens, checkpoint_id,
                        input_cost_micros_per_1k, output_cost_micros_per_1k,
                        context_inspection
                 FROM run_runtime_config
                 JOIN runtime_configurations USING(configuration_hash)
                 WHERE run_id=?1",
                [run_id.as_uuid().as_bytes().as_slice()],
                |row| {
                    Ok((
                        row.get::<_, Option<Vec<u8>>>(0)?,
                        row.get::<_, Option<Vec<u8>>>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, String>(4)?,
                        row.get::<_, String>(5)?,
                        row.get::<_, String>(6)?,
                        row.get::<_, Option<i64>>(7)?,
                        row.get::<_, Option<i64>>(8)?,
                        row.get::<_, Option<i64>>(9)?,
                        row.get::<_, Option<i64>>(10)?,
                        row.get::<_, Option<i64>>(11)?,
                        row.get::<_, Option<i64>>(12)?,
                        row.get::<_, Option<i64>>(13)?,
                        row.get::<_, Option<i64>>(14)?,
                        row.get::<_, Option<Vec<u8>>>(15)?,
                        row.get::<_, i64>(16)?,
                        row.get::<_, i64>(17)?,
                        row.get::<_, Option<String>>(18)?,
                    ))
                },
            )
            .optional()
            .map_err(|error| {
                persistence_error(
                    format!("could not load run runtime configuration: {error}"),
                    true,
                )
            })?;
        row.map(
            |(
                system,
                repository,
                policy_read,
                policy_write,
                policy_command,
                policy_network,
                policy_destructive,
                max_duration_ms,
                max_input_tokens,
                max_output_tokens,
                max_tool_calls,
                max_cost_micros,
                context_window,
                context_max_input_tokens,
                context_reserved_output_tokens,
                checkpoint_id,
                input_cost_micros_per_1k,
                output_cost_micros_per_1k,
                inspection,
            )| {
                Ok(DurableRunRuntimeConfig {
                    system_instructions: system
                        .as_deref()
                        .map(|hash| decode_content(connection, hash))
                        .transpose()?,
                    repository_instructions: repository
                        .as_deref()
                        .map(|hash| decode_content(connection, hash))
                        .transpose()?,
                    approval_policy: ApprovalPolicy {
                        read: decode_policy_decision(&policy_read, "read")?,
                        write: decode_policy_decision(&policy_write, "write")?,
                        command: decode_policy_decision(&policy_command, "command")?,
                        network: decode_policy_decision(&policy_network, "network")?,
                        destructive: decode_policy_decision(&policy_destructive, "destructive")?,
                    },
                    limits: SessionLimits {
                        max_duration_ms: decode_optional_u64(max_duration_ms, "max duration")?,
                        max_input_tokens: decode_optional_u64(
                            max_input_tokens,
                            "max input tokens",
                        )?,
                        max_output_tokens: decode_optional_u64(
                            max_output_tokens,
                            "max output tokens",
                        )?,
                        max_tool_calls: decode_optional_u64(max_tool_calls, "max tool calls")?,
                        max_cost_micros: decode_optional_u64(max_cost_micros, "max cost")?,
                    },
                    context_options: ContextAssemblyOptions {
                        context_window: decode_optional_u64(context_window, "context window")?,
                        max_input_tokens: decode_optional_u64(
                            context_max_input_tokens,
                            "context max input tokens",
                        )?,
                        reserved_output_tokens: decode_optional_u64(
                            context_reserved_output_tokens,
                            "context reserved output tokens",
                        )?,
                    },
                    checkpoint_id: checkpoint_id
                        .as_deref()
                        .map(|bytes| {
                            decode_uuid(bytes, "runtime checkpoint id").map(CheckpointId::from_uuid)
                        })
                        .transpose()?,
                    input_cost_micros_per_1k: decode_counter(
                        input_cost_micros_per_1k,
                        "input cost rate",
                    )?,
                    output_cost_micros_per_1k: decode_counter(
                        output_cost_micros_per_1k,
                        "output cost rate",
                    )?,
                    context_inspection: inspection
                        .as_deref()
                        .map(|payload| decode_json(payload, "run context inspection"))
                        .transpose()?,
                })
            },
        )
        .transpose()
    }

    fn load_run_summaries_matching<P: rusqlite::Params>(
        &self,
        predicate: &str,
        params: P,
        limit: &str,
    ) -> Result<BTreeMap<RunId, DurableRunSummary>> {
        if !self.path.exists() {
            return Ok(BTreeMap::new());
        }
        let connection = self.connection()?;
        Self::load_run_summaries_matching_on(&connection, predicate, params, limit)
    }

    fn load_run_summaries_matching_on<P: rusqlite::Params>(
        connection: &Connection,
        predicate: &str,
        params: P,
        limit: &str,
    ) -> Result<BTreeMap<RunId, DurableRunSummary>> {
        let mut statement = connection
            .prepare(&format!(
                "SELECT run_id, session_id, attempt_id, control_revision, state, started_at, updated_at, completed_at,
                        task, model, summary, input_tokens, output_tokens, cached_input_tokens,
                        tool_calls, cost_micros, elapsed_ms
                 FROM run_summaries {predicate}
                 ORDER BY updated_at DESC, run_id DESC {limit}"
            ))
            .map_err(|error| {
                persistence_error(format!("could not prepare run summaries: {error}"), true)
            })?;
        let rows = statement
            .query_map(params, |row| {
                Ok((
                    (
                        row.get::<_, Vec<u8>>(0)?,
                        row.get::<_, Vec<u8>>(1)?,
                        row.get::<_, Vec<u8>>(2)?,
                        row.get::<_, i64>(3)?,
                        row.get::<_, String>(4)?,
                        row.get::<_, i64>(5)?,
                        row.get::<_, i64>(6)?,
                        row.get::<_, Option<i64>>(7)?,
                    ),
                    (
                        row.get::<_, String>(8)?,
                        row.get::<_, String>(9)?,
                        row.get::<_, Option<String>>(10)?,
                    ),
                    (
                        row.get::<_, i64>(11)?,
                        row.get::<_, i64>(12)?,
                        row.get::<_, i64>(13)?,
                        row.get::<_, i64>(14)?,
                        row.get::<_, i64>(15)?,
                        row.get::<_, i64>(16)?,
                    ),
                ))
            })
            .map_err(|error| {
                persistence_error(format!("could not read run summaries: {error}"), true)
            })?;
        let mut summaries = BTreeMap::new();
        for row in rows {
            let (
                (
                    run_id,
                    session_id,
                    attempt_id,
                    control_revision,
                    state,
                    started,
                    updated,
                    completed,
                ),
                (task, model, summary),
                (
                    input_tokens,
                    output_tokens,
                    cached_input_tokens,
                    tool_calls,
                    cost_micros,
                    elapsed_ms,
                ),
            ) = row.map_err(|error| {
                persistence_error(format!("could not read run summaries: {error}"), true)
            })?;
            let run_id = RunId::from_uuid(decode_uuid(&run_id, "run id")?);
            let session_id = AgentSessionId::from_uuid(decode_uuid(&session_id, "run session id")?);
            let snapshot = AgentRunSnapshot {
                id: run_id,
                session_id,
                attempt_id: RunAttemptId::from_uuid(decode_uuid(&attempt_id, "run attempt id")?),
                control_revision: decode_counter(control_revision, "run control revision")?,
                task,
                model: ModelId::new(model),
                state: parse_run_state(&state)?,
                started_at: decode_timestamp(started)?,
                updated_at: decode_timestamp(updated)?,
                completed_at: completed.map(decode_timestamp).transpose()?,
                summary,
                evidence: Vec::new(),
            };
            let usage = UsageSnapshot {
                input_tokens: decode_counter(input_tokens, "input tokens")?,
                output_tokens: decode_counter(output_tokens, "output tokens")?,
                cached_input_tokens: decode_counter(cached_input_tokens, "cached input tokens")?,
                tool_calls: decode_counter(tool_calls, "tool calls")?,
                cost_micros: decode_counter(cost_micros, "cost")?,
                elapsed_ms: decode_counter(elapsed_ms, "elapsed time")?,
            };
            summaries.insert(
                run_id,
                DurableRunSummary {
                    snapshot,
                    usage,
                    attempts: None,
                    execution_state: None,
                    interactions: None,
                },
            );
        }
        drop(statement);
        let evidence = load_run_evidence_rows(connection, summaries.keys().copied())?;
        for (run_id, links) in evidence {
            if let Some(summary) = summaries.get_mut(&run_id) {
                summary.snapshot.evidence = links;
            }
        }
        Ok(summaries)
    }

    /// Loads a run's typed attempt history independently of its runtime details.
    pub fn load_run_attempts(&self, run_id: RunId) -> Result<Vec<AgentRunAttemptRecord>> {
        if !self.path.exists() {
            return Ok(Vec::new());
        }
        let connection = self.connection()?;
        Self::load_run_attempts_on(&connection, run_id)
    }

    fn load_run_attempts_on(
        connection: &Connection,
        run_id: RunId,
    ) -> Result<Vec<AgentRunAttemptRecord>> {
        let mut statement = connection
            .prepare(
                "SELECT session_id, attempt_id, attempt_number, state, checkpoint_id,
                        started_at, completed_at
                 FROM run_attempts WHERE run_id=?1 ORDER BY attempt_number",
            )
            .map_err(|error| {
                persistence_error(format!("could not prepare run attempts: {error}"), true)
            })?;
        let rows = statement
            .query_map([run_id.as_uuid().as_bytes().as_slice()], |row| {
                Ok((
                    row.get::<_, Vec<u8>>(0)?,
                    row.get::<_, Vec<u8>>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, Option<Vec<u8>>>(4)?,
                    row.get::<_, i64>(5)?,
                    row.get::<_, Option<i64>>(6)?,
                ))
            })
            .map_err(|error| {
                persistence_error(format!("could not read run attempts: {error}"), true)
            })?;
        let mut attempts = Vec::new();
        for row in rows {
            let (session_id, attempt_id, number, state, checkpoint_id, started_at, completed_at) =
                row.map_err(|error| {
                    persistence_error(format!("could not read run attempt row: {error}"), true)
                })?;
            let number = u32::try_from(number).map_err(|_| {
                LoomError::new(
                    ErrorCode::MalformedPayload,
                    "persisted run attempt number is out of range",
                    false,
                )
            })?;
            attempts.push(AgentRunAttemptRecord {
                run_id,
                session_id: AgentSessionId::from_uuid(decode_uuid(
                    &session_id,
                    "run attempt session id",
                )?),
                id: RunAttemptId::from_uuid(decode_uuid(&attempt_id, "run attempt id")?),
                number,
                state: parse_run_state(&state)?,
                checkpoint_id: checkpoint_id
                    .as_deref()
                    .map(|id| decode_uuid(id, "run attempt checkpoint id"))
                    .transpose()?
                    .map(CheckpointId::from_uuid),
                started_at: decode_timestamp(started_at)?,
                completed_at: completed_at.map(decode_timestamp).transpose()?,
            });
        }
        Ok(attempts)
    }

    /// Loads the small continuation record needed to restore a run runtime.
    pub fn load_run_execution_state(
        &self,
        run_id: RunId,
    ) -> Result<Option<AgentExecutionStateRecord>> {
        if !self.path.exists() {
            return Ok(None);
        }
        let connection = self.connection()?;
        Self::load_run_execution_state_on(&connection, run_id)
    }

    fn load_run_execution_state_on(
        connection: &Connection,
        run_id: RunId,
    ) -> Result<Option<AgentExecutionStateRecord>> {
        let row = connection
            .query_row(
                "SELECT session_id, attempt_id, control_revision, state, step_id, step_index,
                        provider_cursor, next_message_id, active_message_id,
                        pending_tool_execution, pending_approval, pending_input, last_failed_call
                 FROM run_execution_state WHERE run_id=?1",
                [run_id.as_uuid().as_bytes().as_slice()],
                |row| {
                    Ok((
                        row.get::<_, Vec<u8>>(0)?,
                        row.get::<_, Vec<u8>>(1)?,
                        row.get::<_, i64>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, Option<Vec<u8>>>(4)?,
                        row.get::<_, i64>(5)?,
                        row.get::<_, i64>(6)?,
                        row.get::<_, i64>(7)?,
                        row.get::<_, Option<i64>>(8)?,
                        row.get::<_, Option<String>>(9)?,
                        row.get::<_, Option<String>>(10)?,
                        row.get::<_, Option<String>>(11)?,
                        row.get::<_, Option<String>>(12)?,
                    ))
                },
            )
            .optional()
            .map_err(|error| {
                persistence_error(format!("could not read run execution state: {error}"), true)
            })?;
        let Some((
            session_id,
            attempt_id,
            control_revision,
            state,
            step_id,
            step_index,
            provider_cursor,
            next_message_id,
            active_message_id,
            pending_tool_execution,
            pending_approval,
            pending_input,
            last_failed_call,
        )) = row
        else {
            return Ok(None);
        };
        let decode_tool_call = |json: Option<String>| {
            json.map(|json| {
                serde_json::from_str(&json).map_err(|error| {
                    LoomError::new(
                        ErrorCode::MalformedPayload,
                        format!("persisted execution tool call is invalid: {error}"),
                        false,
                    )
                })
            })
            .transpose()
        };
        Ok(Some(AgentExecutionStateRecord {
            run_id,
            session_id: AgentSessionId::from_uuid(decode_uuid(
                &session_id,
                "execution-state session id",
            )?),
            attempt_id: RunAttemptId::from_uuid(decode_uuid(
                &attempt_id,
                "execution-state attempt id",
            )?),
            control_revision: u64::try_from(control_revision).map_err(|_| {
                LoomError::new(
                    ErrorCode::MalformedPayload,
                    "persisted execution control revision is negative",
                    false,
                )
            })?,
            state: parse_run_state(&state)?,
            step_id: step_id
                .as_deref()
                .map(|id| decode_uuid(id, "execution-state step id"))
                .transpose()?
                .map(StepId::from_uuid),
            step_index: u32::try_from(step_index).map_err(|_| {
                LoomError::new(
                    ErrorCode::MalformedPayload,
                    "persisted execution step index is out of range",
                    false,
                )
            })?,
            provider_cursor: u64::try_from(provider_cursor).map_err(|_| {
                LoomError::new(
                    ErrorCode::MalformedPayload,
                    "persisted provider cursor is out of range",
                    false,
                )
            })?,
            next_message_id: u64::try_from(next_message_id).map_err(|_| {
                LoomError::new(
                    ErrorCode::MalformedPayload,
                    "persisted next message id is out of range",
                    false,
                )
            })?,
            active_message_id: active_message_id
                .map(u64::try_from)
                .transpose()
                .map_err(|_| {
                    LoomError::new(
                        ErrorCode::MalformedPayload,
                        "persisted active message id is negative",
                        false,
                    )
                })?,
            pending_tool_execution: decode_tool_call(pending_tool_execution)?,
            pending_approval: decode_tool_call(pending_approval)?,
            pending_input,
            last_failed_call: decode_tool_call(last_failed_call)?,
        }))
    }

    /// Loads a run's approval and input interaction history independently of
    /// its summary and runtime details.
    pub fn load_run_interactions(&self, run_id: RunId) -> Result<Vec<AgentInteractionRecord>> {
        if !self.path.exists() {
            return Ok(Vec::new());
        }
        let connection = self.connection()?;
        Self::load_run_interactions_on(&connection, run_id)
    }

    fn load_run_interactions_on(
        connection: &Connection,
        run_id: RunId,
    ) -> Result<Vec<AgentInteractionRecord>> {
        let mut statement = connection
            .prepare(
                "SELECT session_id, interaction_id, attempt_id, control_revision, kind, status,
                        tool_call_id, prompt, decision, created_at, resolved_at
                 FROM run_interactions WHERE run_id=?1
                 ORDER BY attempt_id, control_revision",
            )
            .map_err(|error| {
                persistence_error(format!("could not prepare run interactions: {error}"), true)
            })?;
        let rows = statement
            .query_map([run_id.as_uuid().as_bytes().as_slice()], |row| {
                Ok((
                    row.get::<_, Vec<u8>>(0)?,
                    row.get::<_, Vec<u8>>(1)?,
                    row.get::<_, Vec<u8>>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, String>(5)?,
                    row.get::<_, Option<Vec<u8>>>(6)?,
                    row.get::<_, String>(7)?,
                    row.get::<_, Option<String>>(8)?,
                    row.get::<_, i64>(9)?,
                    row.get::<_, Option<i64>>(10)?,
                ))
            })
            .map_err(|error| {
                persistence_error(format!("could not read run interactions: {error}"), true)
            })?;
        let mut interactions = Vec::new();
        for row in rows {
            let (
                session_id,
                interaction_id,
                attempt_id,
                control_revision,
                kind,
                status,
                tool_call_id,
                prompt,
                decision,
                created_at,
                resolved_at,
            ) = row.map_err(|error| {
                persistence_error(format!("could not read run interaction row: {error}"), true)
            })?;
            let control_revision = u64::try_from(control_revision).map_err(|_| {
                LoomError::new(
                    ErrorCode::MalformedPayload,
                    "persisted interaction revision is negative",
                    false,
                )
            })?;
            interactions.push(AgentInteractionRecord {
                id: InteractionId::from_uuid(decode_uuid(&interaction_id, "interaction id")?),
                run_id,
                session_id: AgentSessionId::from_uuid(decode_uuid(
                    &session_id,
                    "interaction session id",
                )?),
                attempt_id: RunAttemptId::from_uuid(decode_uuid(
                    &attempt_id,
                    "interaction attempt id",
                )?),
                control_revision,
                kind: parse_interaction_kind(&kind)?,
                status: parse_interaction_status(&status)?,
                tool_call_id: tool_call_id
                    .as_deref()
                    .map(|id| decode_uuid(id, "interaction tool call id"))
                    .transpose()?
                    .map(loom_core::ToolCallId::from_uuid),
                prompt,
                decision: decision
                    .as_deref()
                    .map(parse_approval_decision)
                    .transpose()?,
                created_at: decode_timestamp(created_at)?,
                resolved_at: resolved_at.map(decode_timestamp).transpose()?,
            });
        }
        Ok(interactions)
    }

    /// Loads a run's ordered transcript independently of its execution record.
    pub fn load_run_messages(&self, run_id: RunId) -> Result<Vec<DurableRunMessage>> {
        if !self.path.exists() {
            return Ok(Vec::new());
        }
        let connection = self.connection()?;
        let tool_calls_by_message = load_run_message_tool_calls(&connection, run_id, None, None)?;
        let mut statement = connection
            .prepare(
                "SELECT ordinal, role, content_hash, name, tool_call_id
                      FROM run_messages WHERE run_id=?1 ORDER BY ordinal",
            )
            .map_err(|error| {
                persistence_error(format!("could not prepare run messages: {error}"), true)
            })?;
        let rows = statement
            .query_map([run_id.as_uuid().as_bytes().as_slice()], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Option<Vec<u8>>>(2)?,
                    row.get::<_, Option<String>>(3)?,
                    row.get::<_, Option<Vec<u8>>>(4)?,
                ))
            })
            .map_err(|error| {
                persistence_error(format!("could not read run messages: {error}"), true)
            })?;
        rows.map(|row| {
            let (ordinal, role, content_hash, name, tool_call_id) = row.map_err(|error| {
                persistence_error(format!("could not read run message: {error}"), true)
            })?;
            let role = parse_message_role(&role)?;
            let ordinal = u64::try_from(ordinal).map_err(|_| {
                LoomError::new(
                    ErrorCode::MalformedPayload,
                    "persisted message ordinal is negative",
                    false,
                )
            })?;
            let mut content = content_hash
                .map(|hash| decode_content(&connection, &hash))
                .transpose()?
                .unwrap_or_default();
            let fragment_offset = u64::try_from(content.len()).map_err(|_| {
                LoomError::new(
                    ErrorCode::MalformedPayload,
                    "persisted message content is too large",
                    false,
                )
            })?;
            let fragments = load_run_message_fragments(
                &connection,
                run_id,
                ordinal,
                fragment_offset,
                usize::MAX,
            )?;
            if !fragments.is_empty() {
                content.push_str(&String::from_utf8(fragments).map_err(|error| {
                    LoomError::new(
                        ErrorCode::MalformedPayload,
                        format!("persisted assistant fragments are not valid UTF-8: {error}"),
                        false,
                    )
                })?);
            }
            let tool_call_id = decode_optional_tool_call_id(tool_call_id)?;
            Ok(DurableRunMessage {
                role,
                content,
                name,
                tool_call_id,
                tool_calls: tool_calls_by_message
                    .get(&ordinal)
                    .cloned()
                    .unwrap_or_default(),
            })
        })
        .collect()
    }

    /// Loads typed activity metadata and its content-addressed activity data.
    pub fn load_run_activities(&self, run_id: RunId) -> Result<Vec<AgentActivityRecord>> {
        if !self.path.exists() {
            return Ok(Vec::new());
        }
        let connection = self.connection()?;
        Self::load_run_activities_on(&connection, run_id)
    }

    fn load_run_activities_on(
        connection: &Connection,
        run_id: RunId,
    ) -> Result<Vec<AgentActivityRecord>> {
        let mut statement = connection
            .prepare(
                "SELECT ordinal, activity_id, parent_activity_id, step_id, tool_call_id,
                        kind, status, started_at, completed_at, elapsed_ms, data_hash
                 FROM run_activities WHERE run_id=?1 ORDER BY ordinal",
            )
            .map_err(|error| {
                persistence_error(format!("could not prepare run activities: {error}"), true)
            })?;
        let rows = statement
            .query_map([run_id.as_uuid().as_bytes().as_slice()], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, Vec<u8>>(1)?,
                    row.get::<_, Option<Vec<u8>>>(2)?,
                    row.get::<_, Option<Vec<u8>>>(3)?,
                    row.get::<_, Option<Vec<u8>>>(4)?,
                    row.get::<_, String>(5)?,
                    row.get::<_, String>(6)?,
                    row.get::<_, i64>(7)?,
                    row.get::<_, Option<i64>>(8)?,
                    row.get::<_, Option<i64>>(9)?,
                    row.get::<_, Vec<u8>>(10)?,
                ))
            })
            .map_err(|error| {
                persistence_error(format!("could not read run activities: {error}"), true)
            })?
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|error| {
                persistence_error(format!("could not read run activity row: {error}"), true)
            })?;
        drop(statement);

        let calls = Self::load_run_tool_calls_on(connection, run_id)?
            .into_iter()
            .map(|record| (record.call.id, record.call))
            .collect::<BTreeMap<_, _>>();
        let attempts = Self::load_run_tool_attempts_on(connection, run_id)?
            .into_iter()
            .map(|record| (record.id, record.result))
            .collect::<BTreeMap<_, _>>();
        let mut activities = Vec::with_capacity(rows.len());
        for (
            expected_ordinal,
            (
                ordinal,
                activity_id,
                parent_activity_id,
                step_id,
                tool_call_id,
                kind,
                status,
                started_at,
                completed_at,
                elapsed_ms,
                data_hash,
            ),
        ) in rows.into_iter().enumerate()
        {
            if ordinal
                != i64::try_from(expected_ordinal).map_err(|_| {
                    LoomError::new(
                        ErrorCode::MalformedPayload,
                        "persisted activity ordinal is out of range",
                        false,
                    )
                })?
            {
                return Err(LoomError::new(
                    ErrorCode::MalformedPayload,
                    "persisted run activity ordinals are not contiguous",
                    false,
                ));
            }
            let data = decode_content(connection, &data_hash)?;
            let data: AgentActivityData = serde_json::from_str(&data).map_err(|error| {
                LoomError::new(
                    ErrorCode::MalformedPayload,
                    format!("persisted activity data is malformed: {error}"),
                    false,
                )
            })?;
            let tool_call_id = decode_optional_tool_call_id(tool_call_id)?;
            if activity_data_tool_call_id(&data) != tool_call_id {
                return Err(LoomError::new(
                    ErrorCode::MalformedPayload,
                    "persisted activity tool-call index does not match its data",
                    false,
                ));
            }
            let kind = parse_activity_kind(&kind)?;
            if activity_data_kind(&data) != kind {
                return Err(LoomError::new(
                    ErrorCode::MalformedPayload,
                    "persisted activity kind does not match its data",
                    false,
                ));
            }
            let activity_id = ActivityId::from_uuid(decode_uuid(&activity_id, "activity id")?);
            let data = restore_activity_tool_data(data, activity_id, &calls, &attempts)?;
            activities.push(AgentActivityRecord {
                id: activity_id,
                run_id,
                parent_id: decode_optional_activity_id(parent_activity_id)?,
                step_id: decode_optional_step_id(step_id)?,
                kind,
                status: parse_activity_status(&status)?,
                started_at: decode_timestamp(started_at)?,
                completed_at: completed_at.map(decode_timestamp).transpose()?,
                elapsed_ms: elapsed_ms
                    .map(|elapsed| {
                        u64::try_from(elapsed).map_err(|_| {
                            LoomError::new(
                                ErrorCode::MalformedPayload,
                                "persisted activity elapsed time is negative",
                                false,
                            )
                        })
                    })
                    .transpose()?,
                data,
            });
        }
        Ok(activities)
    }

    /// Loads logical tool calls without requiring transcript or activity data.
    pub fn load_run_tool_calls(&self, run_id: RunId) -> Result<Vec<AgentToolCallRecord>> {
        if !self.path.exists() {
            return Ok(Vec::new());
        }
        let connection = self.connection()?;
        Self::load_run_tool_calls_on(&connection, run_id)
    }

    fn load_run_tool_calls_on(
        connection: &Connection,
        run_id: RunId,
    ) -> Result<Vec<AgentToolCallRecord>> {
        let mut statement = connection
            .prepare(
                "SELECT session_id, tool_call_id, name, arguments_hash, created_at
                 FROM run_tool_calls WHERE run_id=?1 ORDER BY created_at, tool_call_id",
            )
            .map_err(|error| {
                persistence_error(format!("could not prepare run tool calls: {error}"), true)
            })?;
        let rows = statement
            .query_map([run_id.as_uuid().as_bytes().as_slice()], |row| {
                Ok((
                    row.get::<_, Vec<u8>>(0)?,
                    row.get::<_, Vec<u8>>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, Vec<u8>>(3)?,
                    row.get::<_, i64>(4)?,
                ))
            })
            .map_err(|error| {
                persistence_error(format!("could not query run tool calls: {error}"), true)
            })?
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|error| {
                persistence_error(format!("could not read run tool call row: {error}"), true)
            })?;
        drop(statement);
        rows.into_iter()
            .map(
                |(session_id, tool_call_id, name, arguments_hash, created_at)| {
                    let arguments = decode_content(connection, &arguments_hash)?;
                    let arguments = serde_json::from_str(&arguments).map_err(|error| {
                        LoomError::new(
                            ErrorCode::MalformedPayload,
                            format!("persisted tool-call arguments are malformed: {error}"),
                            false,
                        )
                    })?;
                    Ok(AgentToolCallRecord {
                        run_id,
                        session_id: AgentSessionId::from_uuid(decode_uuid(
                            &session_id,
                            "tool-call session id",
                        )?),
                        call: loom_model::ToolCall {
                            id: loom_core::ToolCallId::from_uuid(decode_uuid(
                                &tool_call_id,
                                "tool-call id",
                            )?),
                            name,
                            arguments,
                        },
                        created_at: decode_timestamp(created_at)?,
                    })
                },
            )
            .collect()
    }

    /// Loads typed tool execution attempts independently of activity payloads.
    pub fn load_run_tool_attempts(&self, run_id: RunId) -> Result<Vec<AgentToolAttemptRecord>> {
        if !self.path.exists() {
            return Ok(Vec::new());
        }
        let connection = self.connection()?;
        Self::load_run_tool_attempts_on(&connection, run_id)
    }

    fn load_run_tool_attempts_on(
        connection: &Connection,
        run_id: RunId,
    ) -> Result<Vec<AgentToolAttemptRecord>> {
        let mut statement = connection
            .prepare(
                "SELECT session_id, activity_id, tool_call_id, attempt_number, state,
                        started_at, completed_at, result_hash
                 FROM run_tool_attempts WHERE run_id=?1
                 ORDER BY started_at, attempt_number",
            )
            .map_err(|error| {
                persistence_error(
                    format!("could not prepare run tool attempts: {error}"),
                    true,
                )
            })?;
        let rows = statement
            .query_map([run_id.as_uuid().as_bytes().as_slice()], |row| {
                Ok((
                    row.get::<_, Vec<u8>>(0)?,
                    row.get::<_, Vec<u8>>(1)?,
                    row.get::<_, Vec<u8>>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, i64>(5)?,
                    row.get::<_, Option<i64>>(6)?,
                    row.get::<_, Option<Vec<u8>>>(7)?,
                ))
            })
            .map_err(|error| {
                persistence_error(format!("could not query run tool attempts: {error}"), true)
            })?
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|error| {
                persistence_error(
                    format!("could not read run tool attempt row: {error}"),
                    true,
                )
            })?;
        drop(statement);
        rows.into_iter()
            .map(
                |(
                    session_id,
                    activity_id,
                    tool_call_id,
                    attempt_number,
                    state,
                    started_at,
                    completed_at,
                    result_hash,
                )| {
                    let result = result_hash
                        .as_deref()
                        .map(|hash| decode_content(connection, hash))
                        .transpose()?
                        .map(|json| {
                            serde_json::from_str::<ToolResult>(&json).map_err(|error| {
                                LoomError::new(
                                    ErrorCode::MalformedPayload,
                                    format!("persisted tool result is malformed: {error}"),
                                    false,
                                )
                            })
                        })
                        .transpose()?;
                    Ok(AgentToolAttemptRecord {
                        run_id,
                        session_id: AgentSessionId::from_uuid(decode_uuid(
                            &session_id,
                            "tool-attempt session id",
                        )?),
                        id: ActivityId::from_uuid(decode_uuid(
                            &activity_id,
                            "tool-attempt activity id",
                        )?),
                        call_id: loom_core::ToolCallId::from_uuid(decode_uuid(
                            &tool_call_id,
                            "tool-attempt tool-call id",
                        )?),
                        attempt_number: u32::try_from(attempt_number).map_err(|_| {
                            LoomError::new(
                                ErrorCode::MalformedPayload,
                                "persisted tool-attempt number is out of range",
                                false,
                            )
                        })?,
                        state: parse_tool_attempt_state(&state)?,
                        started_at: decode_timestamp(started_at)?,
                        completed_at: completed_at.map(decode_timestamp).transpose()?,
                        result,
                    })
                },
            )
            .collect()
    }

    /// Loads the newest bounded page of message headers. Use the returned
    /// ordinal as `before_ordinal` to continue toward earlier conversation items.
    pub fn load_run_message_page(
        &self,
        run_id: RunId,
        before_ordinal: Option<u64>,
        limit: usize,
    ) -> Result<Vec<DurableRunMessageHeader>> {
        if !(1..=MAX_RUN_MESSAGE_PAGE_SIZE).contains(&limit) {
            return Err(LoomError::invalid_request(format!(
                "run message page size must be between 1 and {MAX_RUN_MESSAGE_PAGE_SIZE}"
            )));
        }
        if !self.path.exists() {
            return Ok(Vec::new());
        }
        let before_ordinal = before_ordinal
            .map(i64::try_from)
            .transpose()
            .map_err(|_| LoomError::invalid_request("message cursor is out of range"))?;
        let limit = i64::try_from(limit)
            .map_err(|_| LoomError::invalid_request("message page size is out of range"))?;
        let connection = self.connection()?;
        let mut statement = connection
            .prepare(
                "SELECT m.ordinal, m.role,
                        MAX(
                            COALESCE(
                                (SELECT raw_size FROM content_objects WHERE hash=m.content_hash),
                                0
                            ),
                            COALESCE(
                                (SELECT MAX(fragment.byte_offset + fragment.byte_length)
                                 FROM run_message_fragments fragment
                                 WHERE fragment.run_id=m.run_id
                                   AND fragment.message_ordinal=m.ordinal),
                                0
                            )
                        ),
                        m.name, m.tool_call_id
                 FROM run_messages m
                 WHERE m.run_id=?1 AND (?2 IS NULL OR m.ordinal < ?2)
                 ORDER BY m.ordinal DESC LIMIT ?3",
            )
            .map_err(|error| {
                persistence_error(format!("could not prepare run message page: {error}"), true)
            })?;
        let rows = statement
            .query_map(
                params![
                    run_id.as_uuid().as_bytes().as_slice(),
                    before_ordinal,
                    limit
                ],
                |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, i64>(2)?,
                        row.get::<_, Option<String>>(3)?,
                        row.get::<_, Option<Vec<u8>>>(4)?,
                    ))
                },
            )
            .map_err(|error| {
                persistence_error(format!("could not query run message page: {error}"), true)
            })?;
        let tool_calls_by_message =
            load_run_message_tool_calls(&connection, run_id, before_ordinal, Some(limit as usize))?;
        rows.map(|row| {
            let (ordinal, role, content_bytes, name, tool_call_id) = row.map_err(|error| {
                persistence_error(format!("could not read run message header: {error}"), true)
            })?;
            let ordinal = u64::try_from(ordinal).map_err(|_| {
                LoomError::new(
                    ErrorCode::MalformedPayload,
                    "persisted message ordinal is negative",
                    false,
                )
            })?;
            Ok(DurableRunMessageHeader {
                ordinal,
                role: parse_message_role(&role)?,
                content_bytes: u64::try_from(content_bytes).map_err(|_| {
                    LoomError::new(
                        ErrorCode::MalformedPayload,
                        "persisted message content size is negative",
                        false,
                    )
                })?,
                name,
                tool_call_id: decode_optional_tool_call_id(tool_call_id)?,
                tool_calls: tool_calls_by_message
                    .get(&ordinal)
                    .cloned()
                    .unwrap_or_default(),
            })
        })
        .collect()
    }

    /// Appends an immutable byte fragment to an assistant message. Repeating an
    /// identical fragment is safe; gaps, overlaps, and conflicting retries fail.
    pub fn append_run_message_fragment(
        &self,
        run_id: RunId,
        session_id: AgentSessionId,
        message_ordinal: u64,
        fragment_ordinal: u64,
        byte_offset: u64,
        content: &[u8],
    ) -> Result<()> {
        if content.is_empty() || content.len() > MAX_MESSAGE_FRAGMENT_BYTES {
            return Err(LoomError::invalid_request(format!(
                "message fragments must contain between 1 and {MAX_MESSAGE_FRAGMENT_BYTES} bytes"
            )));
        }
        if std::str::from_utf8(content).is_err() {
            return Err(LoomError::invalid_request(
                "message fragments must end on UTF-8 character boundaries",
            ));
        }
        let message_ordinal = i64::try_from(message_ordinal)
            .map_err(|_| LoomError::invalid_request("message ordinal is out of range"))?;
        let fragment_ordinal = i64::try_from(fragment_ordinal)
            .map_err(|_| LoomError::invalid_request("fragment ordinal is out of range"))?;
        let byte_offset = i64::try_from(byte_offset)
            .map_err(|_| LoomError::invalid_request("message byte offset is out of range"))?;
        let byte_length = i64::try_from(content.len())
            .map_err(|_| LoomError::invalid_request("message fragment is too large"))?;
        byte_offset
            .checked_add(byte_length)
            .ok_or_else(|| LoomError::invalid_request("message byte range overflows"))?;
        let connection = self.connection_for_write()?;
        let transaction = connection.unchecked_transaction().map_err(|error| {
            persistence_error(
                format!("could not begin message fragment transaction: {error}"),
                true,
            )
        })?;
        let owner: Option<Vec<u8>> = transaction
            .query_row(
                "SELECT session_id FROM run_summaries WHERE run_id=?1",
                [run_id.as_uuid().as_bytes().as_slice()],
                |row| row.get(0),
            )
            .optional()
            .map_err(|error| {
                persistence_error(format!("could not verify message run: {error}"), true)
            })?;
        if owner.as_deref() != Some(session_id.as_uuid().as_bytes().as_slice()) {
            return Err(LoomError::new(
                ErrorCode::WorkspaceAccessDenied,
                "message fragment does not belong to the run's session",
                false,
            ));
        }
        let run_id_bytes = run_id.as_uuid().as_bytes();
        let session_id_bytes = session_id.as_uuid().as_bytes();
        transaction
            .execute(
                "INSERT INTO run_messages(
                    run_id, session_id, ordinal, role, content_hash, name, tool_call_id
                 ) VALUES (?1, ?2, ?3, 'assistant', NULL, NULL, NULL)
                 ON CONFLICT(run_id, ordinal) DO NOTHING",
                params![
                    run_id_bytes.as_slice(),
                    session_id_bytes.as_slice(),
                    message_ordinal
                ],
            )
            .map_err(|error| {
                persistence_error(format!("could not create streamed message: {error}"), true)
            })?;
        let role: String = transaction
            .query_row(
                "SELECT role FROM run_messages WHERE run_id=?1 AND ordinal=?2",
                params![run_id_bytes.as_slice(), message_ordinal],
                |row| row.get(0),
            )
            .map_err(|error| {
                persistence_error(format!("could not verify streamed message: {error}"), true)
            })?;
        if role != "assistant" {
            return Err(LoomError::invalid_request(
                "message fragments can only be appended to assistant messages",
            ));
        }
        let base_size: Option<i64> = transaction
            .query_row(
                "SELECT COALESCE(content.raw_size, 0)
                 FROM run_messages message
                 LEFT JOIN content_objects content ON content.hash=message.content_hash
                 WHERE message.run_id=?1 AND message.ordinal=?2",
                params![run_id_bytes.as_slice(), message_ordinal],
                |row| row.get(0),
            )
            .optional()
            .map_err(|error| {
                persistence_error(
                    format!("could not read message content size: {error}"),
                    true,
                )
            })?;
        let base_offset = base_size.unwrap_or(0);
        let content_hash = store_content(&transaction, content)?;
        let existing: Option<(i64, i64, Vec<u8>)> = transaction
            .query_row(
                "SELECT byte_offset, byte_length, content_hash
                 FROM run_message_fragments
                 WHERE run_id=?1 AND message_ordinal=?2 AND fragment_ordinal=?3",
                params![run_id_bytes.as_slice(), message_ordinal, fragment_ordinal],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()
            .map_err(|error| {
                persistence_error(format!("could not check message fragment: {error}"), true)
            })?;
        if let Some((existing_offset, existing_length, existing_hash)) = existing {
            if existing_offset == byte_offset
                && existing_length == byte_length
                && existing_hash == content_hash
            {
                transaction.commit().map_err(|error| {
                    persistence_error(
                        format!("could not commit repeated message fragment: {error}"),
                        true,
                    )
                })?;
                return Ok(());
            }
            return Err(LoomError::invalid_request(
                "message fragment retry conflicts with the committed fragment",
            ));
        }
        let expected: Option<(i64, i64)> = transaction
            .query_row(
                "SELECT fragment_ordinal, byte_offset + byte_length
                 FROM run_message_fragments
                 WHERE run_id=?1 AND message_ordinal=?2
                 ORDER BY fragment_ordinal DESC LIMIT 1",
                params![run_id_bytes.as_slice(), message_ordinal],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()
            .map_err(|error| {
                persistence_error(
                    format!("could not read message fragment tail: {error}"),
                    true,
                )
            })?;
        if expected.is_some_and(|(ordinal, offset)| {
            ordinal.checked_add(1) != Some(fragment_ordinal) || offset != byte_offset
        }) || (expected.is_none() && (fragment_ordinal != 0 || byte_offset != base_offset))
        {
            return Err(LoomError::invalid_request(
                "message fragments must be appended contiguously in order",
            ));
        }
        transaction
            .execute(
                "INSERT INTO run_message_fragments(
                    run_id, message_ordinal, fragment_ordinal, byte_offset, byte_length, content_hash
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    run_id_bytes.as_slice(),
                    message_ordinal,
                    fragment_ordinal,
                    byte_offset,
                    byte_length,
                    content_hash
                ],
            )
            .map_err(|error| {
                persistence_error(format!("could not append message fragment: {error}"), true)
            })?;
        transaction.commit().map_err(|error| {
            persistence_error(format!("could not commit message fragment: {error}"), true)
        })
    }

    /// Returns the next sequence and byte offset for appending to an assistant
    /// message, including fragments committed before a process restart.
    pub fn next_run_message_fragment_position(
        &self,
        run_id: RunId,
        message_ordinal: u64,
    ) -> Result<(u64, u64)> {
        let message_ordinal = i64::try_from(message_ordinal)
            .map_err(|_| LoomError::invalid_request("message ordinal is out of range"))?;
        let connection = self.connection()?;
        let tail: Option<(i64, i64)> = connection
            .query_row(
                "SELECT fragment_ordinal, byte_offset + byte_length
                 FROM run_message_fragments
                 WHERE run_id=?1 AND message_ordinal=?2
                 ORDER BY fragment_ordinal DESC LIMIT 1",
                params![run_id.as_uuid().as_bytes().as_slice(), message_ordinal],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()
            .map_err(|error| {
                persistence_error(
                    format!("could not read message fragment cursor: {error}"),
                    true,
                )
            })?;
        if let Some((fragment_ordinal, byte_offset)) = tail {
            return Ok((
                u64::try_from(fragment_ordinal)
                    .ok()
                    .and_then(|ordinal| ordinal.checked_add(1))
                    .ok_or_else(|| {
                        LoomError::new(
                            ErrorCode::MalformedPayload,
                            "persisted message fragment ordinal is out of range",
                            false,
                        )
                    })?,
                u64::try_from(byte_offset).map_err(|_| {
                    LoomError::new(
                        ErrorCode::MalformedPayload,
                        "persisted message fragment offset is out of range",
                        false,
                    )
                })?,
            ));
        }
        let base_size: Option<i64> = connection
            .query_row(
                "SELECT COALESCE(content.raw_size, 0)
                 FROM run_messages message
                 LEFT JOIN content_objects content ON content.hash=message.content_hash
                 WHERE message.run_id=?1 AND message.ordinal=?2",
                params![run_id.as_uuid().as_bytes().as_slice(), message_ordinal],
                |row| row.get(0),
            )
            .optional()
            .map_err(|error| {
                persistence_error(
                    format!("could not read message content size: {error}"),
                    true,
                )
            })?;
        let byte_offset = base_size.unwrap_or(0);
        Ok((
            0,
            u64::try_from(byte_offset).map_err(|_| {
                LoomError::new(
                    ErrorCode::MalformedPayload,
                    "persisted message content size is negative",
                    false,
                )
            })?,
        ))
    }

    /// Reads a bounded byte range from a completed message or its committed
    /// fragments, without returning the rest of the conversation.
    pub fn load_run_message_content_range(
        &self,
        run_id: RunId,
        message_ordinal: u64,
        byte_offset: u64,
        length: usize,
    ) -> Result<Vec<u8>> {
        if length > MAX_CONTENT_RANGE_BYTES {
            return Err(LoomError::invalid_request(format!(
                "message content range exceeds {MAX_CONTENT_RANGE_BYTES} bytes"
            )));
        }
        let message_ordinal = i64::try_from(message_ordinal)
            .map_err(|_| LoomError::invalid_request("message ordinal is out of range"))?;
        let byte_offset_bytes = byte_offset;
        let byte_offset = i64::try_from(byte_offset_bytes)
            .map_err(|_| LoomError::invalid_request("message byte offset is out of range"))?;
        let connection = self.connection()?;
        let content_hash: Option<Vec<u8>> = connection
            .query_row(
                "SELECT content_hash FROM run_messages WHERE run_id=?1 AND ordinal=?2",
                params![run_id.as_uuid().as_bytes().as_slice(), message_ordinal],
                |row| row.get(0),
            )
            .optional()
            .map_err(|error| {
                persistence_error(format!("could not locate message content: {error}"), true)
            })?
            .ok_or_else(|| LoomError::invalid_request("run message does not exist"))?;
        if let Some(content_hash) = content_hash {
            let start = usize::try_from(byte_offset)
                .map_err(|_| LoomError::invalid_request("message byte offset is out of range"))?;
            let content_size: i64 = connection
                .query_row(
                    "SELECT raw_size FROM content_objects WHERE hash=?1",
                    [&content_hash],
                    |row| row.get(0),
                )
                .map_err(|error| {
                    persistence_error(
                        format!("could not read message content size: {error}"),
                        true,
                    )
                })?;
            let content_size = usize::try_from(content_size).map_err(|_| {
                LoomError::new(
                    ErrorCode::MalformedPayload,
                    "persisted message content size is invalid",
                    false,
                )
            })?;
            let mut output = load_content_range(&connection, &content_hash, start, length)?;
            if output.len() < length {
                let content_end = u64::try_from(content_size).map_err(|_| {
                    LoomError::new(
                        ErrorCode::MalformedPayload,
                        "persisted message content is too large",
                        false,
                    )
                })?;
                let fragment_start = byte_offset_bytes.max(content_end);
                output.extend(load_run_message_fragments(
                    &connection,
                    run_id,
                    u64::try_from(message_ordinal).map_err(|_| {
                        LoomError::invalid_request("message ordinal is out of range")
                    })?,
                    fragment_start,
                    length - output.len(),
                )?);
            }
            return Ok(output);
        }
        load_run_message_fragments(
            &connection,
            run_id,
            u64::try_from(message_ordinal)
                .map_err(|_| LoomError::invalid_request("message ordinal is out of range"))?,
            byte_offset_bytes,
            length,
        )
    }

    pub fn list_filesystem_sessions(&self) -> Result<Vec<AgentSessionId>> {
        if !self.path.exists() {
            return Ok(Vec::new());
        }
        let connection = self.connection()?;
        let mut statement = connection
            .prepare("SELECT session_id FROM session_filesystems ORDER BY session_id")
            .map_err(|error| {
                persistence_error(format!("could not prepare filesystem index: {error}"), true)
            })?;
        let rows = statement
            .query_map([], |row| row.get::<_, Vec<u8>>(0))
            .map_err(|error| {
                persistence_error(format!("could not read filesystem index: {error}"), true)
            })?;
        rows.map(|row| {
            row.map_err(|error| {
                persistence_error(format!("could not read filesystem index: {error}"), true)
            })
            .and_then(|id| decode_uuid(&id, "filesystem session id"))
            .map(AgentSessionId::from_uuid)
        })
        .collect()
    }

    /// Appends newly observed filesystem changes and enforces bounded retention.
    pub fn save_filesystem_changes(
        &self,
        session_id: AgentSessionId,
        next_sequence: EventSequence,
        changes: &[SessionFilesystemChange],
    ) -> Result<()> {
        let connection = self.connection_for_write()?;
        let transaction = connection.unchecked_transaction().map_err(|error| {
            persistence_error(
                format!("could not begin filesystem change write: {error}"),
                true,
            )
        })?;
        let next_sequence = encode_counter(next_sequence.value(), "filesystem sequence")?;
        transaction
            .execute(
                "INSERT INTO filesystem_change_state(session_id, next_sequence)
                 VALUES (?1, ?2)
                 ON CONFLICT(session_id) DO UPDATE SET
                    next_sequence=MAX(filesystem_change_state.next_sequence, excluded.next_sequence)",
                params![session_id.as_uuid().as_bytes().as_slice(), next_sequence],
            )
            .map_err(|error| {
                persistence_error(format!("could not update filesystem sequence: {error}"), true)
            })?;
        save_filesystem_change_rows(&transaction, session_id, changes)?;
        transaction.commit().map_err(|error| {
            persistence_error(
                format!("could not commit filesystem changes: {error}"),
                true,
            )
        })
    }

    /// Loads a bounded filesystem change page without restoring the workspace.
    pub fn load_filesystem_changes_page(
        &self,
        session_id: AgentSessionId,
        after: Option<EventSequence>,
        limit: usize,
    ) -> Result<DurableFilesystemChangesPage> {
        if !(1..=MAX_FILESYSTEM_CHANGE_PAGE_SIZE).contains(&limit) {
            return Err(LoomError::invalid_request(format!(
                "filesystem change page size must be between 1 and {MAX_FILESYSTEM_CHANGE_PAGE_SIZE}"
            )));
        }
        if !self.path.exists() {
            return Ok(DurableFilesystemChangesPage {
                changes: Vec::new(),
                truncated: false,
            });
        }
        let after = after
            .map(|sequence| encode_counter(sequence.value(), "filesystem sequence"))
            .transpose()?;
        let connection = self.connection()?;
        let first_retained: Option<i64> = connection
            .query_row(
                "SELECT MIN(sequence) FROM filesystem_changes WHERE session_id=?1",
                [session_id.as_uuid().as_bytes().as_slice()],
                |row| row.get(0),
            )
            .map_err(|error| {
                persistence_error(
                    format!("could not inspect filesystem change retention: {error}"),
                    true,
                )
            })?;
        let limit_plus_one = i64::try_from(limit + 1).map_err(|_| {
            LoomError::invalid_request("filesystem change page size is out of range")
        })?;
        let mut statement = connection
            .prepare(
                "SELECT sequence, path, kind, revision FROM filesystem_changes
                 WHERE session_id=?1 AND (?2 IS NULL OR sequence>?2)
                 ORDER BY sequence DESC LIMIT ?3",
            )
            .map_err(|error| {
                persistence_error(
                    format!("could not prepare filesystem change page: {error}"),
                    true,
                )
            })?;
        let rows = statement
            .query_map(
                params![
                    session_id.as_uuid().as_bytes().as_slice(),
                    after,
                    limit_plus_one
                ],
                |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, Option<String>>(3)?,
                    ))
                },
            )
            .map_err(|error| {
                persistence_error(
                    format!("could not query filesystem change page: {error}"),
                    true,
                )
            })?;
        let mut rows = rows
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|error| {
                persistence_error(
                    format!("could not read filesystem change page: {error}"),
                    true,
                )
            })?;
        let has_more = rows.len() > limit;
        if has_more {
            rows.truncate(limit);
        }
        rows.reverse();
        let history_pruned = after
            .zip(first_retained)
            .is_some_and(|(cursor, first)| first > cursor.saturating_add(1));
        let mut changes = Vec::with_capacity(rows.len());
        for (sequence, path, kind, revision) in rows {
            changes.push(SessionFilesystemChange {
                sequence: EventSequence::new(decode_counter(sequence, "filesystem sequence")?),
                session_id,
                path,
                kind: parse_workspace_change_kind(&kind)?,
                revision,
            });
        }
        Ok(DurableFilesystemChangesPage {
            changes,
            truncated: history_pruned || has_more,
        })
    }

    pub fn load_filesystem_record(
        &self,
        session_id: AgentSessionId,
    ) -> Result<Option<DurableFilesystemRecord>> {
        if !self.path.exists() {
            return Ok(None);
        }
        let connection = self.connection()?;
        let row = connection
            .query_row(
                "SELECT root, control, payload_hash, raw_size, payload_codec, payload
                 FROM session_filesystems WHERE session_id=?1",
                [session_id.as_uuid().as_bytes().as_slice()],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, Vec<u8>>(2)?,
                        row.get::<_, i64>(3)?,
                        row.get::<_, i64>(4)?,
                        row.get::<_, Vec<u8>>(5)?,
                    ))
                },
            )
            .optional()
            .map_err(|error| {
                persistence_error(format!("could not read filesystem record: {error}"), true)
            })?;
        let Some((root, control, hash, raw_size, codec, payload)) = row else {
            return Ok(None);
        };
        let control = parse_workspace_control(&control)?;
        let raw_size = usize::try_from(raw_size).map_err(|_| {
            LoomError::new(
                ErrorCode::MalformedPayload,
                "persisted filesystem payload has an invalid size",
                false,
            )
        })?;
        if raw_size > MAX_CONTENT_BYTES {
            return Err(LoomError::new(
                ErrorCode::MalformedPayload,
                "persisted filesystem payload exceeds the maximum supported size",
                false,
            ));
        }
        let raw = match codec {
            0 => payload,
            1 => {
                let mut decoded = Vec::with_capacity(raw_size);
                ZlibDecoder::new(payload.as_slice())
                    .take((raw_size as u64).saturating_add(1))
                    .read_to_end(&mut decoded)
                    .map_err(|error| {
                        LoomError::new(
                            ErrorCode::MalformedPayload,
                            format!("persisted filesystem payload is malformed: {error}"),
                            false,
                        )
                    })?;
                decoded
            }
            _ => {
                return Err(LoomError::new(
                    ErrorCode::MalformedPayload,
                    "persisted filesystem payload uses an unsupported codec",
                    false,
                ));
            }
        };
        if raw.len() != raw_size || Sha256::digest(&raw).as_slice() != hash {
            return Err(LoomError::new(
                ErrorCode::MalformedPayload,
                "persisted filesystem payload failed its length or hash check",
                false,
            ));
        }
        let mut payload: Value = serde_json::from_slice(&raw).map_err(|error| {
            LoomError::new(
                ErrorCode::MalformedPayload,
                format!("persisted filesystem payload is malformed: {error}"),
                false,
            )
        })?;
        let filesystem = payload.get("filesystem").ok_or_else(|| {
            LoomError::new(
                ErrorCode::MalformedPayload,
                "persisted filesystem payload has no workspace snapshot",
                false,
            )
        })?;
        let payload_root = filesystem.get("root").and_then(Value::as_str);
        let payload_control = filesystem.get("control").and_then(Value::as_str);
        if payload_root != Some(root.as_str())
            || payload_control != Some(workspace_control_name(control))
            || filesystem.get("session_id").and_then(Value::as_str)
                != Some(session_id.to_string().as_str())
        {
            return Err(LoomError::new(
                ErrorCode::MalformedPayload,
                "filesystem index columns do not match the payload",
                false,
            ));
        }
        let sequence = connection
            .query_row(
                "SELECT next_sequence FROM filesystem_change_state WHERE session_id=?1",
                [session_id.as_uuid().as_bytes().as_slice()],
                |row| row.get::<_, i64>(0),
            )
            .optional()
            .map_err(|error| {
                persistence_error(
                    format!("could not read filesystem sequence state: {error}"),
                    true,
                )
            })?
            .map(|sequence| decode_counter(sequence, "filesystem sequence"))
            .transpose()?
            .unwrap_or(0);
        let payload_sequence = filesystem
            .get("next_sequence")
            .and_then(Value::as_u64)
            .ok_or_else(|| {
                LoomError::new(
                    ErrorCode::MalformedPayload,
                    "persisted filesystem sequence is missing",
                    false,
                )
            })?;
        if sequence < payload_sequence {
            return Err(LoomError::new(
                ErrorCode::MalformedPayload,
                "filesystem sequence index is behind its payload",
                false,
            ));
        }
        if sequence > payload_sequence {
            payload["filesystem"]["next_sequence"] = serde_json::json!(sequence);
        }
        let checkpoint_rows = {
            let mut statement = connection
                .prepare(
                    "SELECT checkpoint_id, label, created_at FROM checkpoints
                     WHERE session_id=?1 ORDER BY created_at, checkpoint_id",
                )
                .map_err(|error| {
                    persistence_error(format!("could not prepare checkpoint list: {error}"), true)
                })?;
            let rows = statement
                .query_map([session_id.as_uuid().as_bytes().as_slice()], |row| {
                    Ok((
                        row.get::<_, Vec<u8>>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, i64>(2)?,
                    ))
                })
                .map_err(|error| {
                    persistence_error(format!("could not read checkpoint list: {error}"), true)
                })?;
            rows.collect::<std::result::Result<Vec<_>, _>>()
                .map_err(|error| {
                    persistence_error(format!("could not read checkpoint list: {error}"), true)
                })?
        };
        let mut checkpoints = Vec::with_capacity(checkpoint_rows.len());
        for (checkpoint_id, label, created_at) in checkpoint_rows {
            let checkpoint_id =
                CheckpointId::from_uuid(decode_uuid(&checkpoint_id, "checkpoint id")?);
            let file_rows = {
                let mut statement = connection
                    .prepare(
                        "SELECT path, existed, revision, expected_revision, content_hash
                         FROM checkpoint_files WHERE session_id=?1 AND checkpoint_id=?2
                         ORDER BY path",
                    )
                    .map_err(|error| {
                        persistence_error(
                            format!("could not prepare checkpoint files: {error}"),
                            true,
                        )
                    })?;
                let rows = statement
                    .query_map(
                        params![
                            session_id.as_uuid().as_bytes().as_slice(),
                            checkpoint_id.as_uuid().as_bytes().as_slice()
                        ],
                        |row| {
                            Ok((
                                row.get::<_, String>(0)?,
                                row.get::<_, i64>(1)?,
                                row.get::<_, String>(2)?,
                                row.get::<_, String>(3)?,
                                row.get::<_, Vec<u8>>(4)?,
                            ))
                        },
                    )
                    .map_err(|error| {
                        persistence_error(format!("could not read checkpoint files: {error}"), true)
                    })?;
                rows.collect::<std::result::Result<Vec<_>, _>>()
                    .map_err(|error| {
                        persistence_error(format!("could not read checkpoint files: {error}"), true)
                    })?
            };
            let mut files = BTreeMap::new();
            for (path, existed, revision, expected_revision, content_hash) in file_rows {
                let content = decode_content(&connection, &content_hash)?;
                files.insert(
                    path,
                    CheckpointFile {
                        existed: existed != 0,
                        content,
                        revision,
                        expected_revision,
                    },
                );
            }
            checkpoints.push(Checkpoint {
                id: checkpoint_id,
                session_id,
                label,
                created_at: decode_timestamp(created_at)?,
                files,
            });
        }
        let edit_rows = {
            let mut statement = connection
                .prepare(
                    "SELECT edit_id, path, before_hash, after_revision, source FROM filesystem_edits
                     WHERE session_id=?1 ORDER BY edit_id",
                )
                .map_err(|error| {
                    persistence_error(format!("could not prepare filesystem edits: {error}"), true)
                })?;
            let rows = statement
                .query_map([session_id.as_uuid().as_bytes().as_slice()], |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, Option<Vec<u8>>>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, String>(4)?,
                    ))
                })
                .map_err(|error| {
                    persistence_error(format!("could not read filesystem edits: {error}"), true)
                })?;
            rows.collect::<std::result::Result<Vec<_>, _>>()
                .map_err(|error| {
                    persistence_error(format!("could not read filesystem edits: {error}"), true)
                })?
        };
        let mut edits = Vec::with_capacity(edit_rows.len());
        for (edit_id, path, before_hash, after_revision, source) in edit_rows {
            let before_bytes = before_hash
                .as_deref()
                .map(|hash| load_content_range(&connection, hash, 0, MAX_CONTENT_BYTES))
                .transpose()?;
            let before = before_bytes
                .as_ref()
                .and_then(|bytes| String::from_utf8(bytes.clone()).ok());
            edits.push(DurableFilesystemEdit {
                id: u64::try_from(edit_id).map_err(|_| {
                    LoomError::new(
                        ErrorCode::MalformedPayload,
                        "persisted edit id is invalid",
                        false,
                    )
                })?,
                path,
                before,
                before_bytes,
                after_revision,
                source: parse_workspace_control(&source)?,
            });
        }
        let changes = Vec::new();
        let repositories = {
            let mut statement = connection
                .prepare(
                    "SELECT repository_id, source, path, revision, attached_at
                     FROM session_repositories WHERE session_id=?1 ORDER BY repository_id",
                )
                .map_err(|error| {
                    persistence_error(format!("could not prepare repository rows: {error}"), true)
                })?;
            let rows = statement
                .query_map([session_id.as_uuid().as_bytes().as_slice()], |row| {
                    Ok((
                        row.get::<_, Vec<u8>>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, Option<String>>(3)?,
                        row.get::<_, i64>(4)?,
                    ))
                })
                .map_err(|error| {
                    persistence_error(format!("could not read repository rows: {error}"), true)
                })?;
            let rows = rows
                .collect::<std::result::Result<Vec<_>, _>>()
                .map_err(|error| {
                    persistence_error(format!("could not read repository rows: {error}"), true)
                })?;
            let mut repositories = BTreeMap::new();
            for (id, source, path, revision, attached_at) in rows {
                let id = RepositoryId::from_uuid(decode_uuid(&id, "repository id")?);
                repositories.insert(
                    id,
                    SessionRepository {
                        id,
                        source,
                        path,
                        revision,
                        attached_at: decode_timestamp(attached_at)?,
                    },
                );
            }
            repositories
        };
        let directories = {
            let mut statement = connection
                .prepare("SELECT source, path FROM session_directories WHERE session_id=?1 ORDER BY ordinal")
                .map_err(|error| persistence_error(format!("could not prepare mounted directories: {error}"), true))?;
            let rows = statement
                .query_map([session_id.as_uuid().as_bytes().as_slice()], |row| {
                    Ok(SessionDirectory {
                        source: row.get(0)?,
                        path: row.get(1)?,
                    })
                })
                .map_err(|error| {
                    persistence_error(format!("could not read mounted directories: {error}"), true)
                })?;
            rows.collect::<std::result::Result<Vec<_>, _>>()
                .map_err(|error| {
                    persistence_error(format!("could not read mounted directories: {error}"), true)
                })?
        };
        Ok(Some(DurableFilesystemRecord {
            session_id,
            root,
            control,
            checkpoints,
            edits,
            changes,
            repositories,
            directories,
            payload,
            delta: None,
        }))
    }

    /// Loads the bounded reconnect feed from sequence-indexed records.
    pub fn load_feed_state(&self) -> Result<Option<DurableFeedState>> {
        if !self.path.exists() {
            return Ok(None);
        }
        let connection = self.connection()?;
        let meta = connection
            .query_row(
                "SELECT next_sequence, retention_limit FROM feed_store_meta WHERE singleton=1",
                [],
                |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)),
            )
            .optional()
            .map_err(|error| {
                persistence_error(format!("could not read event feed: {error}"), true)
            })?;
        let Some((next_sequence, retention_limit)) = meta else {
            return Ok(None);
        };
        let largest_stream_bytes: i64 = connection
            .query_row(
                "SELECT COALESCE(MAX(stream_bytes), 0) FROM (
                    SELECT SUM(length(payload)) AS stream_bytes
                    FROM feed_events GROUP BY session_id
                    UNION ALL
                    SELECT SUM(length(payload)) AS stream_bytes
                    FROM workspace_feed_events GROUP BY workspace_id
                 )",
                [],
                |row| row.get(0),
            )
            .map_err(|error| {
                persistence_error(format!("could not validate event feed size: {error}"), true)
            })?;
        let total_feed_bytes: i64 = connection
            .query_row(
                "SELECT COALESCE(SUM(length(payload)), 0) FROM feed_events",
                [],
                |row| row.get(0),
            )
            .map_err(|error| {
                persistence_error(
                    format!("could not validate total event feed size: {error}"),
                    true,
                )
            })?;
        if largest_stream_bytes < 0
            || largest_stream_bytes > MAX_DURABLE_FEED_SESSION_BYTES as i64
            || total_feed_bytes < 0
            || total_feed_bytes > MAX_DURABLE_FEED_TOTAL_BYTES as i64
        {
            return Err(LoomError::new(
                ErrorCode::MalformedPayload,
                "persisted event feed exceeds its byte limit",
                false,
            ));
        }
        let mut statement = connection
            .prepare(
                "SELECT sequence, session_id, payload_codec, payload
                 FROM feed_events ORDER BY sequence",
            )
            .map_err(|error| {
                persistence_error(format!("could not prepare event feed: {error}"), true)
            })?;
        let rows = statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, Vec<u8>>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, Vec<u8>>(3)?,
                ))
            })
            .map_err(|error| {
                persistence_error(format!("could not read event feed: {error}"), true)
            })?;
        let mut events = Vec::new();
        for row in rows {
            let (sequence, session_id, payload_codec, payload) = row.map_err(|error| {
                persistence_error(format!("could not read event feed: {error}"), true)
            })?;
            let payload = match payload_codec {
                0 => payload,
                1 => {
                    let mut decoded = Vec::new();
                    ZlibDecoder::new(payload.as_slice())
                        .take(MAX_FEED_EVENT_BYTES as u64 + 1)
                        .read_to_end(&mut decoded)
                        .map_err(|error| {
                            LoomError::new(
                                ErrorCode::MalformedPayload,
                                format!("persisted event feed entry is malformed: {error}"),
                                false,
                            )
                        })?;
                    decoded
                }
                _ => {
                    return Err(LoomError::new(
                        ErrorCode::MalformedPayload,
                        "persisted event feed entry uses an unsupported codec",
                        false,
                    ));
                }
            };
            if payload.len() > MAX_FEED_EVENT_BYTES {
                return Err(LoomError::new(
                    ErrorCode::MalformedPayload,
                    "persisted event feed entry exceeds the maximum supported size",
                    false,
                ));
            }
            let event: ServerEventEnvelope = serde_json::from_slice(&payload).map_err(|error| {
                LoomError::new(
                    ErrorCode::MalformedPayload,
                    format!("persisted event feed entry is malformed: {error}"),
                    false,
                )
            })?;
            if sequence < 0
                || event.sequence.value() != sequence as u64
                || event.session_id.as_uuid().as_bytes().as_slice() != session_id.as_slice()
            {
                return Err(LoomError::new(
                    ErrorCode::MalformedPayload,
                    "persisted event feed index does not match its payload",
                    false,
                ));
            }
            events.push(event);
        }
        let next_sequence = u64::try_from(next_sequence).map_err(|_| {
            LoomError::new(
                ErrorCode::MalformedPayload,
                "persisted event cursor is negative",
                false,
            )
        })?;
        let retention_limit = usize::try_from(retention_limit).map_err(|_| {
            LoomError::new(
                ErrorCode::MalformedPayload,
                "persisted event retention is invalid",
                false,
            )
        })?;
        let mut events_per_session = BTreeMap::<AgentSessionId, usize>::new();
        for event in &events {
            *events_per_session.entry(event.session_id).or_default() += 1;
        }
        let workspace_events = load_all_workspace_events(&connection)?;
        let mut events_per_workspace = BTreeMap::<WorkspaceId, usize>::new();
        let mut seen_sequences = events
            .iter()
            .map(|event| event.sequence)
            .collect::<BTreeSet<_>>();
        for event in &workspace_events {
            *events_per_workspace.entry(event.workspace_id).or_default() += 1;
            if !seen_sequences.insert(event.sequence) || event.sequence.value() > next_sequence {
                return Err(LoomError::new(
                    ErrorCode::MalformedPayload,
                    "persisted session and workspace event sequences overlap or exceed the cursor",
                    false,
                ));
            }
        }
        if events_per_session
            .values()
            .any(|count| *count > retention_limit)
            || events_per_workspace
                .values()
                .any(|count| *count > retention_limit)
            || events
                .last()
                .is_some_and(|event| event.sequence.value() > next_sequence)
        {
            return Err(LoomError::new(
                ErrorCode::MalformedPayload,
                "persisted event feed exceeds its cursor or retention limit",
                false,
            ));
        }
        Ok(Some(DurableFeedState {
            next_sequence: EventSequence::new(next_sequence),
            retention_limit,
            events,
            workspace_events,
        }))
    }

    /// Loads only the global feed cursor metadata for startup.
    pub fn load_feed_header(&self) -> Result<Option<DurableFeedHeader>> {
        if !self.path.exists() {
            return Ok(None);
        }
        let connection = self.connection()?;
        let meta = connection
            .query_row(
                "SELECT next_sequence, retention_limit FROM feed_store_meta WHERE singleton=1",
                [],
                |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)),
            )
            .optional()
            .map_err(|error| {
                persistence_error(format!("could not read event feed header: {error}"), true)
            })?;
        meta.map(|(next_sequence, retention_limit)| {
            Ok(DurableFeedHeader {
                next_sequence: EventSequence::new(decode_counter(next_sequence, "event cursor")?),
                retention_limit: usize::try_from(retention_limit).map_err(|_| {
                    LoomError::new(
                        ErrorCode::MalformedPayload,
                        "persisted event retention is invalid",
                        false,
                    )
                })?,
            })
        })
        .transpose()
    }

    /// Loads lightweight retained-boundary metadata for one session stream.
    pub fn load_feed_session_cursor(
        &self,
        session_id: AgentSessionId,
    ) -> Result<Option<DurableFeedSessionCursor>> {
        if !self.path.exists() {
            return Ok(None);
        }
        let connection = self.connection()?;
        Self::load_feed_session_cursor_on(&connection, session_id)
    }

    fn load_feed_session_cursor_on(
        connection: &Connection,
        session_id: AgentSessionId,
    ) -> Result<Option<DurableFeedSessionCursor>> {
        let cursor = connection
            .query_row(
                "SELECT first_sequence, latest_sequence, pruned_through,
                        (SELECT MIN(sequence) FROM feed_events WHERE session_id=?1)
                 FROM feed_session_meta WHERE session_id=?1",
                [session_id.as_uuid().as_bytes().as_slice()],
                |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, i64>(2)?,
                        row.get::<_, Option<i64>>(3)?,
                    ))
                },
            )
            .optional()
            .map_err(|error| {
                persistence_error(format!("could not read session feed cursor: {error}"), true)
            })?;
        cursor
            .map(|(first, latest, pruned, oldest)| {
                Ok(DurableFeedSessionCursor {
                    first_sequence: EventSequence::new(decode_counter(
                        first,
                        "first event sequence",
                    )?),
                    latest_sequence: EventSequence::new(decode_counter(
                        latest,
                        "latest event sequence",
                    )?),
                    pruned_through: EventSequence::new(decode_counter(
                        pruned,
                        "pruned event sequence",
                    )?),
                    oldest_retained_sequence: oldest
                        .map(|sequence| decode_counter(sequence, "oldest event sequence"))
                        .transpose()?
                        .map(EventSequence::new),
                })
            })
            .transpose()
    }

    /// Loads lightweight retained-boundary metadata for one workspace stream.
    pub fn load_feed_workspace_cursor(
        &self,
        workspace_id: WorkspaceId,
    ) -> Result<Option<DurableFeedWorkspaceCursor>> {
        if !self.path.exists() {
            return Ok(None);
        }
        let connection = self.connection()?;
        let cursor = connection
            .query_row(
                "SELECT first_sequence, latest_sequence, pruned_through,
                        (SELECT MIN(sequence) FROM (
                            SELECT sequence FROM feed_events WHERE workspace_id=?1
                            UNION ALL
                            SELECT sequence FROM workspace_feed_events WHERE workspace_id=?1
                        ))
                 FROM feed_workspace_meta WHERE workspace_id=?1",
                [workspace_id.as_uuid().as_bytes().as_slice()],
                |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, i64>(2)?,
                        row.get::<_, Option<i64>>(3)?,
                    ))
                },
            )
            .optional()
            .map_err(|error| {
                persistence_error(
                    format!("could not read workspace feed cursor: {error}"),
                    true,
                )
            })?;
        cursor
            .map(|(first, latest, pruned, oldest)| {
                Ok(DurableFeedWorkspaceCursor {
                    first_sequence: EventSequence::new(decode_counter(
                        first,
                        "first event sequence",
                    )?),
                    latest_sequence: EventSequence::new(decode_counter(
                        latest,
                        "latest event sequence",
                    )?),
                    pruned_through: EventSequence::new(decode_counter(
                        pruned,
                        "pruned event sequence",
                    )?),
                    oldest_retained_sequence: oldest
                        .map(|sequence| decode_counter(sequence, "oldest event sequence"))
                        .transpose()?
                        .map(EventSequence::new),
                })
            })
            .transpose()
    }

    /// Loads retained events belonging to one workspace after a global sequence cursor.
    pub fn load_feed_workspace_events_since(
        &self,
        workspace_id: WorkspaceId,
        after_sequence: Option<EventSequence>,
    ) -> Result<Vec<WorkspaceFeedEvent>> {
        let mut events = self
            .load_feed_events_by_workspace(workspace_id, after_sequence)?
            .into_iter()
            .map(WorkspaceFeedEvent::Session)
            .collect::<Vec<_>>();
        events.extend(self.load_workspace_only_events_since(workspace_id, after_sequence)?);
        events.sort_by_key(|event| match event {
            WorkspaceFeedEvent::Session(event) => event.sequence,
            WorkspaceFeedEvent::Workspace(event) => event.sequence,
        });
        Ok(events)
    }

    fn load_workspace_only_events_since(
        &self,
        workspace_id: WorkspaceId,
        after_sequence: Option<EventSequence>,
    ) -> Result<Vec<WorkspaceFeedEvent>> {
        if !self.path.exists() {
            return Ok(Vec::new());
        }
        let connection = self.connection()?;
        let after = after_sequence
            .map(|sequence| sequence.value() as i64)
            .unwrap_or(0);
        let mut statement = connection
            .prepare(
                "SELECT sequence, payload_codec, payload FROM workspace_feed_events
             WHERE workspace_id=?1 AND sequence>?2 ORDER BY sequence ASC",
            )
            .map_err(|error| {
                persistence_error(
                    format!("could not prepare workspace-only feed page: {error}"),
                    true,
                )
            })?;
        let rows = statement
            .query_map(
                params![workspace_id.as_uuid().as_bytes().as_slice(), after],
                |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, Vec<u8>>(2)?,
                    ))
                },
            )
            .map_err(|error| {
                persistence_error(
                    format!("could not read workspace-only feed page: {error}"),
                    true,
                )
            })?;
        rows.map(|row| {
            let (sequence, codec, payload) = row.map_err(|error| {
                persistence_error(
                    format!("could not read workspace-only feed row: {error}"),
                    true,
                )
            })?;
            let payload = decode_feed_payload(codec, payload)?;
            let event: WorkspaceEventEnvelope =
                serde_json::from_slice(&payload).map_err(|error| {
                    LoomError::new(
                        ErrorCode::MalformedPayload,
                        format!("persisted workspace event is malformed: {error}"),
                        false,
                    )
                })?;
            if sequence <= 0
                || event.sequence.value() != sequence as u64
                || event.workspace_id != workspace_id
            {
                return Err(LoomError::new(
                    ErrorCode::MalformedPayload,
                    "persisted workspace event index does not match its payload",
                    false,
                ));
            }
            Ok(WorkspaceFeedEvent::Workspace(event))
        })
        .collect()
    }

    fn load_feed_events_by_workspace(
        &self,
        workspace_id: WorkspaceId,
        after_sequence: Option<EventSequence>,
    ) -> Result<Vec<ServerEventEnvelope>> {
        if !self.path.exists() {
            return Ok(Vec::new());
        }
        let connection = self.connection()?;
        let after = after_sequence
            .map(|sequence| {
                i64::try_from(sequence.value()).map_err(|_| {
                    LoomError::invalid_request("event cursor exceeds SQLite's integer range")
                })
            })
            .transpose()?;
        let mut statement = connection
            .prepare(
                "SELECT sequence, session_id, payload_codec, payload FROM feed_events
             WHERE workspace_id=?1 AND sequence>?2 ORDER BY sequence ASC",
            )
            .map_err(|error| {
                persistence_error(
                    format!("could not prepare workspace event feed page: {error}"),
                    true,
                )
            })?;
        let rows = statement
            .query_map(
                params![
                    workspace_id.as_uuid().as_bytes().as_slice(),
                    after.unwrap_or(0)
                ],
                |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, Vec<u8>>(1)?,
                        row.get::<_, i64>(2)?,
                        row.get::<_, Vec<u8>>(3)?,
                    ))
                },
            )
            .map_err(|error| {
                persistence_error(
                    format!("could not read workspace event feed page: {error}"),
                    true,
                )
            })?;
        rows.map(|row| {
            let (sequence, session_id, codec, payload) = row.map_err(|error| {
                persistence_error(
                    format!("could not read workspace event feed row: {error}"),
                    true,
                )
            })?;
            decode_feed_event(sequence, session_id, codec, payload)
        })
        .collect()
    }

    /// Loads a session's retained events after a cursor without hydrating other streams.
    pub fn load_feed_events_since(
        &self,
        session_id: Option<AgentSessionId>,
        after_sequence: Option<EventSequence>,
    ) -> Result<Vec<ServerEventEnvelope>> {
        self.load_feed_events_query(session_id, after_sequence, None, false)
    }

    /// Loads a bounded tail of one session's retained events.
    pub fn load_recent_feed_events(
        &self,
        session_id: AgentSessionId,
        limit: usize,
    ) -> Result<Vec<ServerEventEnvelope>> {
        self.load_feed_events_query(Some(session_id), None, Some(limit), true)
    }

    fn load_feed_events_query(
        &self,
        session_id: Option<AgentSessionId>,
        after_sequence: Option<EventSequence>,
        limit: Option<usize>,
        descending: bool,
    ) -> Result<Vec<ServerEventEnvelope>> {
        if !self.path.exists() || limit == Some(0) {
            return Ok(Vec::new());
        }
        let limit = limit
            .map(|limit| {
                i64::try_from(limit).map_err(|_| {
                    LoomError::invalid_request("event feed limit exceeds SQLite's integer range")
                })
            })
            .transpose()?
            .unwrap_or(-1);
        let connection = self.connection()?;
        let mut bindings = Vec::with_capacity(3);
        let mut predicates = Vec::with_capacity(2);
        if let Some(session_id) = session_id {
            bindings.push(SqlValue::Blob(session_id.as_uuid().as_bytes().to_vec()));
            predicates.push(format!("session_id=?{}", bindings.len()));
        }
        if let Some(sequence) = after_sequence {
            let sequence = i64::try_from(sequence.value()).map_err(|_| {
                LoomError::invalid_request("event cursor exceeds SQLite's integer range")
            })?;
            bindings.push(SqlValue::Integer(sequence));
            predicates.push(format!("sequence>?{}", bindings.len()));
        }
        let where_clause = if predicates.is_empty() {
            String::new()
        } else {
            format!("WHERE {}", predicates.join(" AND "))
        };
        bindings.push(SqlValue::Integer(limit));
        let ordering = if descending { "DESC" } else { "ASC" };
        let sql = format!(
            "SELECT sequence, session_id, payload_codec, payload FROM feed_events
             {where_clause} ORDER BY sequence {ordering} LIMIT ?{}",
            bindings.len()
        );
        let mut statement = connection.prepare(&sql).map_err(|error| {
            persistence_error(format!("could not prepare event feed page: {error}"), true)
        })?;
        let rows = statement
            .query_map(rusqlite::params_from_iter(bindings.iter()), |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, Vec<u8>>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, Vec<u8>>(3)?,
                ))
            })
            .map_err(|error| {
                persistence_error(format!("could not read event feed page: {error}"), true)
            })?;
        let mut events = rows
            .map(|row| {
                let (sequence, session_id, codec, payload) = row.map_err(|error| {
                    persistence_error(format!("could not read event feed row: {error}"), true)
                })?;
                decode_feed_event(sequence, session_id, codec, payload)
            })
            .collect::<Result<Vec<_>>>()?;
        if descending {
            events.reverse();
        }
        Ok(events)
    }

    pub fn save_state(&self, write: DurableStateWrite<'_>) -> Result<()> {
        let connection = self.connection_for_write()?;
        let transaction = connection.unchecked_transaction().map_err(|error| {
            persistence_error(
                format!("could not begin persistence transaction: {error}"),
                true,
            )
        })?;
        save_session_rows(&transaction, write.sessions)?;
        if let Some(workspaces) = write.workspaces {
            save_workspace_rows(&transaction, workspaces)?;
        }
        if let Some(settings) = write.settings {
            save_session_settings_rows(&transaction, settings)?;
        }
        if let Some(workspace_configs) = write.workspace_configs {
            save_workspace_config_rows(&transaction, workspace_configs)?;
        }
        if let Some(providers) = write.providers {
            save_provider_config_rows(&transaction, &providers.configs)?;
            save_provider_health_rows(&transaction, &providers.health)?;
        }
        if let Some(usage) = write.usage {
            save_usage_totals(&transaction, usage)?;
        }
        if let Some(idempotency) = write.idempotency {
            save_idempotency_rows(&transaction, idempotency)?;
        }
        if let Some(run_summaries) = write.run_summaries {
            save_run_summary_rows(&transaction, run_summaries)?;
            save_run_attempt_rows(&transaction, run_summaries)?;
            save_run_execution_state_rows(&transaction, run_summaries)?;
        }
        if let Some(runtime_configs) = write.run_runtime_configs {
            save_run_runtime_config_rows(&transaction, runtime_configs)?;
        }
        if let Some(context_checkpoints) = write.run_context_checkpoints {
            save_run_context_checkpoint_rows(&transaction, context_checkpoints)?;
        }
        if let Some(run_plans) = write.run_plans {
            save_run_plan_rows(&transaction, run_plans, write.run_summaries)?;
        }
        if let Some(run_activities) = write.run_activities {
            save_run_activity_rows(&transaction, run_activities)?;
            save_run_tool_rows(&transaction, run_activities, write.run_summaries)?;
        }
        if let Some(run_messages) = write.run_messages {
            save_run_message_rows(&transaction, run_messages, None, true)?;
        }
        if let Some(filesystems) = write.filesystem_records {
            save_filesystem_records(&transaction, filesystems)?;
        }
        if let Some(feed) = write.feed {
            save_feed_rows(&transaction, feed)?;
        }
        transaction.commit().map_err(|error| {
            persistence_error(
                format!("could not commit persistence transaction: {error}"),
                true,
            )
        })
    }

    /// Atomically persists startup recovery updates without rewriting unrelated catalogs.
    pub fn save_recovery_updates(
        &self,
        summaries: &BTreeMap<RunId, DurableRunSummary>,
        feed: &DurableFeedState,
    ) -> Result<()> {
        let connection = self.connection_for_write()?;
        let transaction = connection.unchecked_transaction().map_err(|error| {
            persistence_error(
                format!("could not begin recovery update transaction: {error}"),
                true,
            )
        })?;
        save_run_summary_rows(&transaction, summaries)?;
        save_run_attempt_rows(&transaction, summaries)?;
        save_run_execution_state_rows(&transaction, summaries)?;
        save_feed_rows(&transaction, feed)?;
        transaction.commit().map_err(|error| {
            persistence_error(
                format!("could not commit recovery update transaction: {error}"),
                true,
            )
        })
    }

    /// Atomically checkpoints one worker run, its owner filesystem, and the
    /// captured event-feed state without enumerating or pruning other catalogs.
    pub fn save_run_checkpoint(&self, write: DurableRunCheckpointWrite<'_>) -> Result<()> {
        let run_id = write.summary.snapshot.id;
        if write.session.id != write.summary.snapshot.session_id
            || write
                .activities
                .iter()
                .any(|activity| activity.run_id != run_id)
            || write.filesystem.is_some_and(|filesystem| {
                filesystem.session_id != write.summary.snapshot.session_id
            })
            || write
                .message_delta
                .is_some_and(|delta| delta.reset && delta.start_ordinal != 0)
        {
            return Err(LoomError::invalid_request(
                "run checkpoint records must belong to the same run and session, and transcript resets must start at ordinal zero",
            ));
        }
        let summaries = BTreeMap::from([(run_id, write.summary.clone())]);
        let runtime_configs = BTreeMap::from([(run_id, write.runtime_config.clone())]);
        let context_checkpoints = BTreeMap::from([(run_id, write.context_checkpoint.cloned())]);
        let plans = BTreeMap::from([(run_id, write.plan.clone())]);
        let connection = self.connection_for_write()?;
        let transaction = connection.unchecked_transaction().map_err(|error| {
            persistence_error(
                format!("could not begin run checkpoint transaction: {error}"),
                true,
            )
        })?;
        let messages = if let Some(delta) = write.message_delta {
            if delta.reset {
                transaction
                    .execute(
                        "DELETE FROM run_messages WHERE run_id=?1",
                        [run_id.as_uuid().as_bytes().as_slice()],
                    )
                    .map_err(|error| {
                        persistence_error(format!("could not reset run transcript: {error}"), true)
                    })?;
                // Fragments for base rows cascade, while orphaned streamed rows
                // need explicit removal during a transcript-generation reset.
                transaction
                    .execute(
                        "DELETE FROM run_message_fragments WHERE run_id=?1",
                        [run_id.as_uuid().as_bytes().as_slice()],
                    )
                    .map_err(|error| {
                        persistence_error(
                            format!("could not reset run transcript fragments: {error}"),
                            true,
                        )
                    })?;
            }
            BTreeMap::from([(run_id, delta.messages.clone())])
        } else {
            BTreeMap::from([(run_id, write.messages.to_vec())])
        };
        let message_offsets = write.message_delta.map(|delta| {
            BTreeMap::from([(run_id, if delta.reset { 0 } else { delta.start_ordinal })])
        });
        let activities = BTreeMap::from([(run_id, write.activities.to_vec())]);
        save_session_checkpoint_row(&transaction, write.session, write.session_next_sequence)?;
        save_run_summary_rows(&transaction, &summaries)?;
        save_run_attempt_rows(&transaction, &summaries)?;
        save_run_execution_state_rows(&transaction, &summaries)?;
        save_run_runtime_config_rows(&transaction, &runtime_configs)?;
        save_run_context_checkpoint_rows(&transaction, &context_checkpoints)?;
        save_run_plan_rows(&transaction, &plans, Some(&summaries))?;
        if let Some(activity_deltas) = write.activity_deltas {
            save_run_activity_deltas(&transaction, run_id, activity_deltas, &summaries)?;
        } else {
            save_run_activity_rows(&transaction, &activities)?;
            save_run_tool_rows(&transaction, &activities, Some(&summaries))?;
        }
        save_run_message_rows(
            &transaction,
            &messages,
            message_offsets.as_ref(),
            write.message_delta.is_none(),
        )?;
        if let Some(filesystem) = write.filesystem {
            save_filesystem_records(&transaction, std::slice::from_ref(filesystem))?;
        }
        if write.prune_feed {
            save_feed_rows(&transaction, write.feed)?;
        } else {
            save_feed_rows_incremental(&transaction, write.feed)?;
        }
        transaction.commit().map_err(|error| {
            persistence_error(
                format!("could not commit run checkpoint transaction: {error}"),
                true,
            )
        })
    }

    /// Collects a bounded batch of content objects and blobs queued by reference changes.
    /// Ordinary writes process smaller batches; callers can repeat this method to drain a
    /// backlog without scanning all stored content on every transaction.
    pub fn collect_garbage(&self, max_candidates: usize) -> Result<()> {
        if !(1..=MAX_MANUAL_CONTENT_GC_CANDIDATES).contains(&max_candidates) {
            return Err(LoomError::invalid_request(format!(
                "content garbage-collection batch must be between 1 and {MAX_MANUAL_CONTENT_GC_CANDIDATES}"
            )));
        }
        let connection = self.connection_for_write()?;
        let transaction = connection.unchecked_transaction().map_err(|error| {
            persistence_error(
                format!("could not begin content garbage-collection transaction: {error}"),
                true,
            )
        })?;
        collect_unused_content(&transaction, max_candidates)?;
        transaction.commit().map_err(|error| {
            persistence_error(
                format!("could not commit content garbage collection: {error}"),
                true,
            )
        })
    }

    fn connection(&self) -> Result<CachedConnection<'_>> {
        self.cached_connection(false)
    }

    fn connection_for_write(&self) -> Result<CachedConnection<'_>> {
        if let Some(parent) = self
            .path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            fs::create_dir_all(parent).map_err(|error| {
                persistence_error(
                    format!(
                        "could not create persistence directory '{}': {error}",
                        parent.display()
                    ),
                    true,
                )
            })?;
        }
        self.cached_connection(true)
    }

    fn cached_connection(&self, create: bool) -> Result<CachedConnection<'_>> {
        let mut cached = self.connection.lock().map_err(|_| {
            LoomError::new(
                ErrorCode::Persistence,
                "persistence connection lock was poisoned",
                true,
            )
        })?;
        if cached.is_none() {
            if !create && !self.path.is_file() {
                return Err(persistence_error(
                    "persistence database does not exist".to_owned(),
                    false,
                ));
            }
            let connection = Connection::open(&self.path).map_err(|error| {
                persistence_error(
                    format!("could not open persistence database: {error}"),
                    true,
                )
            })?;
            connection
                .busy_timeout(std::time::Duration::from_secs(5))
                .map_err(|error| {
                    persistence_error(format!("could not configure persistence: {error}"), true)
                })?;
            connection
                .execute_batch("PRAGMA foreign_keys = ON; PRAGMA synchronous = FULL;")
                .map_err(|error| {
                    persistence_error(format!("could not configure persistence: {error}"), true)
                })?;
            initialize_schema(&connection)?;
            *cached = Some(connection);
        }
        Ok(CachedConnection(cached))
    }
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
    if database_version != 0 {
        return Err(LoomError::new(
            ErrorCode::MalformedPayload,
            format!("unsupported persistence database version {database_version}"),
            false,
        ));
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
        return Err(LoomError::new(
            ErrorCode::MalformedPayload,
            "this database uses an unsupported persistence format; this version starts with a new database and does not import or modify existing state",
            false,
        ));
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
                SELECT 1 FROM run_message_fragments
                WHERE run_message_fragments.content_hash=content_objects.hash
             ) AND NOT EXISTS (
                SELECT 1 FROM run_activities
                WHERE run_activities.data_hash=content_objects.hash
             ) AND NOT EXISTS (
                SELECT 1 FROM run_tool_calls
                WHERE run_tool_calls.arguments_hash=content_objects.hash
             ) AND NOT EXISTS (
                SELECT 1 FROM run_tool_attempts
                WHERE run_tool_attempts.result_hash=content_objects.hash
             ) AND NOT EXISTS (
                SELECT 1 FROM run_message_tool_calls
                WHERE run_message_tool_calls.arguments_hash=content_objects.hash
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
    let total: Option<i64> = connection
        .query_row(
            "SELECT MAX(byte_offset + byte_length) FROM run_message_fragments
             WHERE run_id=?1 AND message_ordinal=?2",
            params![run_id.as_uuid().as_bytes().as_slice(), message_ordinal],
            |row| row.get(0),
        )
        .map_err(|error| {
            persistence_error(
                format!("could not read message fragment size: {error}"),
                true,
            )
        })?;
    let Some(total) = total else {
        return Ok(Vec::new());
    };
    let total = u64::try_from(total).map_err(|_| {
        LoomError::new(
            ErrorCode::MalformedPayload,
            "persisted message fragment size is negative",
            false,
        )
    })?;
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
    let start_i64 = i64::try_from(byte_offset)
        .map_err(|_| LoomError::invalid_request("message byte offset is out of range"))?;
    let end_i64 = i64::try_from(end).map_err(|_| {
        LoomError::new(
            ErrorCode::MalformedPayload,
            "message size is out of range",
            false,
        )
    })?;
    let mut statement = connection
        .prepare(
            "SELECT byte_offset, byte_length, content_hash
             FROM run_message_fragments
             WHERE run_id=?1 AND message_ordinal=?2
               AND byte_offset < ?4 AND byte_offset + byte_length > ?3
             ORDER BY byte_offset",
        )
        .map_err(|error| {
            persistence_error(
                format!("could not prepare message content range: {error}"),
                true,
            )
        })?;
    let rows = statement
        .query_map(
            params![
                run_id.as_uuid().as_bytes().as_slice(),
                message_ordinal,
                start_i64,
                end_i64
            ],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, Vec<u8>>(2)?,
                ))
            },
        )
        .map_err(|error| {
            persistence_error(
                format!("could not query message content range: {error}"),
                true,
            )
        })?;
    let mut cursor = byte_offset;
    for row in rows {
        let (fragment_offset, fragment_length, hash) = row.map_err(|error| {
            persistence_error(
                format!("could not read message content range: {error}"),
                true,
            )
        })?;
        let fragment_offset = u64::try_from(fragment_offset).map_err(|_| {
            LoomError::new(
                ErrorCode::MalformedPayload,
                "persisted fragment offset is negative",
                false,
            )
        })?;
        let fragment_length = usize::try_from(fragment_length).map_err(|_| {
            LoomError::new(
                ErrorCode::MalformedPayload,
                "persisted fragment length is invalid",
                false,
            )
        })?;
        let fragment = decode_content(connection, &hash)?.into_bytes();
        if fragment.len() != fragment_length || fragment_offset > cursor {
            return Err(LoomError::new(
                ErrorCode::MalformedPayload,
                "persisted message fragments contain a gap or invalid length",
                false,
            ));
        }
        let fragment_end = fragment_offset
            .checked_add(u64::try_from(fragment_length).map_err(|_| {
                LoomError::new(
                    ErrorCode::MalformedPayload,
                    "persisted fragment length is out of range",
                    false,
                )
            })?)
            .ok_or_else(|| {
                LoomError::new(
                    ErrorCode::MalformedPayload,
                    "persisted message fragment range overflows",
                    false,
                )
            })?;
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
        output.extend_from_slice(&fragment[local_start..local_end]);
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
    let fragments = {
        let mut statement = transaction
            .prepare(
                "SELECT byte_offset, content_hash
                 FROM run_message_fragments
                 WHERE run_id=?1 AND message_ordinal=?2
                 ORDER BY fragment_ordinal",
            )
            .map_err(|error| {
                persistence_error(
                    format!("could not prepare message fragment consolidation: {error}"),
                    true,
                )
            })?;
        let rows = statement
            .query_map(
                params![run_id.as_uuid().as_bytes().as_slice(), message_ordinal],
                |row| Ok((row.get::<_, i64>(0)?, row.get::<_, Vec<u8>>(1)?)),
            )
            .map_err(|error| {
                persistence_error(
                    format!("could not query message fragments for consolidation: {error}"),
                    true,
                )
            })?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|error| {
                persistence_error(
                    format!("could not read message fragments for consolidation: {error}"),
                    true,
                )
            })?
    };
    if fragments.is_empty() {
        return Ok(None);
    }
    for (byte_offset, content_hash) in fragments {
        let byte_offset = usize::try_from(byte_offset).map_err(|_| {
            LoomError::new(
                ErrorCode::MalformedPayload,
                "persisted fragment offset is out of range",
                false,
            )
        })?;
        let fragment = decode_content(transaction, &content_hash)?.into_bytes();
        let Some(fragment_end) = byte_offset.checked_add(fragment.len()) else {
            return Err(LoomError::new(
                ErrorCode::MalformedPayload,
                "persisted fragment range overflows",
                false,
            ));
        };
        if canonical_content.get(byte_offset..fragment_end) != Some(fragment.as_slice()) {
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
            "DELETE FROM run_message_fragments
             WHERE run_id=?1 AND message_ordinal=?2",
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

fn load_run_message_tool_calls(
    connection: &Connection,
    run_id: RunId,
    before_ordinal: Option<i64>,
    limit: Option<usize>,
) -> Result<BTreeMap<u64, Vec<loom_model::ToolCall>>> {
    let mut calls = BTreeMap::<u64, Vec<loom_model::ToolCall>>::new();
    let limit = limit.map(|limit| i64::try_from(limit).unwrap_or(i64::MAX));
    let mut statement = connection
        .prepare(
            "SELECT c.message_ordinal,c.tool_call_id,c.name,c.arguments_hash
         FROM run_message_tool_calls c
         WHERE c.run_id=?1 AND (?2 IS NULL OR c.message_ordinal<?2)
           AND (?3 IS NULL OR c.message_ordinal IN (
             SELECT ordinal FROM run_messages
             WHERE run_id=?1 AND (?2 IS NULL OR ordinal<?2)
             ORDER BY ordinal DESC LIMIT ?3
           ))
         ORDER BY c.message_ordinal DESC,c.call_ordinal",
        )
        .map_err(|error| {
            persistence_error(
                format!("could not prepare run message tool calls: {error}"),
                true,
            )
        })?;
    let rows = statement
        .query_map(
            params![
                run_id.as_uuid().as_bytes().as_slice(),
                before_ordinal,
                limit
            ],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, Vec<u8>>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, Vec<u8>>(3)?,
                ))
            },
        )
        .map_err(|error| {
            persistence_error(
                format!("could not read run message tool calls: {error}"),
                true,
            )
        })?;
    for row in rows {
        let (ordinal, id, name, hash) = row.map_err(|error| {
            persistence_error(
                format!("could not read run message tool call: {error}"),
                true,
            )
        })?;
        let ordinal = u64::try_from(ordinal).map_err(|_| {
            LoomError::new(
                ErrorCode::MalformedPayload,
                "persisted message ordinal is negative",
                false,
            )
        })?;
        let arguments = decode_content(connection, &hash)?;
        let arguments = serde_json::from_str(&arguments).map_err(|error| {
            LoomError::new(
                ErrorCode::MalformedPayload,
                format!("persisted tool arguments are malformed: {error}"),
                false,
            )
        })?;
        calls
            .entry(ordinal)
            .or_default()
            .push(loom_model::ToolCall {
                id: loom_core::ToolCallId::from_uuid(decode_uuid(&id, "tool-call id")?),
                name,
                arguments,
            });
    }
    Ok(calls)
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
    transaction
        .execute_batch(
            "CREATE TEMP TABLE IF NOT EXISTS _loom_wanted_run_message_tool_calls (
                run_id BLOB NOT NULL, message_ordinal INTEGER NOT NULL,
                call_ordinal INTEGER NOT NULL, tool_call_id BLOB NOT NULL,
                name TEXT NOT NULL, arguments_hash BLOB NOT NULL,
                PRIMARY KEY(run_id, message_ordinal, call_ordinal),
                UNIQUE(run_id, message_ordinal, tool_call_id)
             ) WITHOUT ROWID, STRICT;
             DELETE FROM _loom_wanted_run_message_tool_calls;",
        )
        .map_err(|error| {
            persistence_error(
                format!("could not stage run message tool calls: {error}"),
                true,
            )
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
            for (call_ordinal, call) in message.tool_calls.iter().enumerate() {
                if call.name.len() > 4096 {
                    return Err(LoomError::invalid_request(
                        "run message tool name exceeds the maximum supported size",
                    ));
                }
                let arguments = serde_json::to_vec(&call.arguments).map_err(|error| {
                    persistence_error(format!("could not encode tool arguments: {error}"), false)
                })?;
                if arguments.len() > 1024 * 1024 {
                    return Err(LoomError::invalid_request(
                        "run message tool arguments exceed the maximum supported size",
                    ));
                }
                let arguments_hash = store_content(transaction, &arguments)?;
                let call_ordinal = i64::try_from(call_ordinal).map_err(|_| {
                    LoomError::new(
                        ErrorCode::Persistence,
                        "too many tool calls in message",
                        false,
                    )
                })?;
                transaction
                    .execute(
                        "INSERT INTO _loom_wanted_run_message_tool_calls
                     (run_id,message_ordinal,call_ordinal,tool_call_id,name,arguments_hash)
                     VALUES (?1,?2,?3,?4,?5,?6)",
                        params![
                            run_id_bytes.as_slice(),
                            ordinal,
                            call_ordinal,
                            call.id.as_uuid().as_bytes().as_slice(),
                            call.name,
                            arguments_hash
                        ],
                    )
                    .map_err(|error| {
                        persistence_error(
                            format!("could not stage run message tool call: {error}"),
                            true,
                        )
                    })?;
            }
            transaction
                .execute(
                    "INSERT INTO run_messages(run_id, session_id, ordinal, role, content_hash,
                    name, tool_call_id)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
                 ON CONFLICT(run_id, ordinal) DO UPDATE SET
                    session_id=excluded.session_id, role=excluded.role,
                    content_hash=excluded.content_hash, name=excluded.name,
                    tool_call_id=excluded.tool_call_id
                 WHERE run_messages.session_id IS NOT excluded.session_id
                    OR run_messages.role IS NOT excluded.role
                    OR run_messages.content_hash IS NOT excluded.content_hash
                    OR run_messages.name IS NOT excluded.name
                    OR run_messages.tool_call_id IS NOT excluded.tool_call_id",
                    params![
                        run_id_bytes.as_slice(),
                        session_id,
                        ordinal,
                        message_role_name(message.role),
                        content_hash,
                        message.name,
                        tool_call_id
                    ],
                )
                .map_err(|error| {
                    persistence_error(format!("could not save run message: {error}"), true)
                })?;
            transaction
                .execute(
                    "DELETE FROM run_message_tool_calls AS saved
                 WHERE saved.run_id=?1 AND saved.message_ordinal=?2 AND NOT EXISTS (
                    SELECT 1 FROM _loom_wanted_run_message_tool_calls wanted
                    WHERE wanted.run_id=saved.run_id
                      AND wanted.message_ordinal=saved.message_ordinal
                      AND wanted.call_ordinal=saved.call_ordinal
                      AND wanted.tool_call_id=saved.tool_call_id
                      AND wanted.name=saved.name
                      AND wanted.arguments_hash=saved.arguments_hash
                 )",
                    params![run_id_bytes.as_slice(), ordinal],
                )
                .map_err(|error| {
                    persistence_error(
                        format!("could not prune run message tool calls: {error}"),
                        true,
                    )
                })?;
            transaction
                .execute(
                    "INSERT INTO run_message_tool_calls
                 (run_id,message_ordinal,call_ordinal,tool_call_id,name,arguments_hash)
                 SELECT run_id,message_ordinal,call_ordinal,tool_call_id,name,arguments_hash
                 FROM _loom_wanted_run_message_tool_calls
                 WHERE run_id=?1 AND message_ordinal=?2
                 ON CONFLICT(run_id,message_ordinal,call_ordinal) DO NOTHING",
                    params![run_id_bytes.as_slice(), ordinal],
                )
                .map_err(|error| {
                    persistence_error(
                        format!("could not save run message tool calls: {error}"),
                        true,
                    )
                })?;
            if fragments_match == Some(true) {
                delete_run_message_fragments(transaction, *run_id, ordinal)?;
            }
        }
        if prune_missing {
            transaction
                .execute(
                    "DELETE FROM run_messages WHERE run_id=?1 AND NOT EXISTS (
                SELECT 1 FROM _loom_wanted_run_messages wanted
                WHERE wanted.run_id=run_messages.run_id AND wanted.ordinal=run_messages.ordinal
                ) AND NOT EXISTS (
                   SELECT 1 FROM run_message_fragments fragment
                   WHERE fragment.run_id=run_messages.run_id
                     AND fragment.message_ordinal=run_messages.ordinal
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

fn save_run_runtime_config_rows(
    transaction: &Transaction<'_>,
    configs: &BTreeMap<RunId, DurableRunRuntimeConfig>,
) -> Result<()> {
    for (run_id, config) in configs {
        let context_inspection = config
            .context_inspection
            .as_ref()
            .map(serde_json::to_string)
            .transpose()
            .map_err(|error| {
                persistence_error(
                    format!("could not encode run context inspection: {error}"),
                    false,
                )
            })?;
        if config
            .system_instructions
            .as_ref()
            .is_some_and(|value| value.len() > MAX_RUN_RUNTIME_CONFIG_BYTES)
            || config
                .repository_instructions
                .as_ref()
                .is_some_and(|value| value.len() > MAX_RUN_RUNTIME_CONFIG_BYTES)
            || context_inspection
                .as_ref()
                .is_some_and(|value| value.len() > MAX_RUN_RUNTIME_CONFIG_BYTES)
        {
            return Err(LoomError::new(
                ErrorCode::Persistence,
                "run runtime configuration exceeds its maximum supported size",
                false,
            ));
        }
        let system_instructions_hash = config
            .system_instructions
            .as_deref()
            .map(|value| store_content(transaction, value.as_bytes()))
            .transpose()?;
        let repository_instructions_hash = config
            .repository_instructions
            .as_deref()
            .map(|value| store_content(transaction, value.as_bytes()))
            .transpose()?;
        let limits = &config.limits;
        let context = &config.context_options;
        let max_duration_ms = encode_optional_counter(limits.max_duration_ms, "max duration")?;
        let max_input_tokens =
            encode_optional_counter(limits.max_input_tokens, "max input tokens")?;
        let max_output_tokens =
            encode_optional_counter(limits.max_output_tokens, "max output tokens")?;
        let max_tool_calls = encode_optional_counter(limits.max_tool_calls, "max tool calls")?;
        let max_cost_micros = encode_optional_counter(limits.max_cost_micros, "max cost")?;
        let context_window = encode_optional_counter(context.context_window, "context window")?;
        let context_max_input_tokens =
            encode_optional_counter(context.max_input_tokens, "context max input tokens")?;
        let context_reserved_output_tokens = encode_optional_counter(
            context.reserved_output_tokens,
            "context reserved output tokens",
        )?;
        let input_cost_micros_per_1k =
            encode_counter(config.input_cost_micros_per_1k, "input cost rate")?;
        let output_cost_micros_per_1k =
            encode_counter(config.output_cost_micros_per_1k, "output cost rate")?;
        let policy_decisions = [
            encode_policy_decision(config.approval_policy.read),
            encode_policy_decision(config.approval_policy.write),
            encode_policy_decision(config.approval_policy.command),
            encode_policy_decision(config.approval_policy.network),
            encode_policy_decision(config.approval_policy.destructive),
        ];
        let config_identity = serde_json::to_vec(&(
            system_instructions_hash.as_deref(),
            repository_instructions_hash.as_deref(),
            policy_decisions,
            [
                max_duration_ms,
                max_input_tokens,
                max_output_tokens,
                max_tool_calls,
                max_cost_micros,
            ],
            [
                context_window,
                context_max_input_tokens,
                context_reserved_output_tokens,
            ],
            config
                .checkpoint_id
                .map(|id| id.as_uuid().as_bytes().to_vec()),
            input_cost_micros_per_1k,
            output_cost_micros_per_1k,
        ))
        .map_err(|error| {
            persistence_error(
                format!("could not encode run runtime configuration identity: {error}"),
                false,
            )
        })?;
        let configuration_hash = Sha256::digest(config_identity).to_vec();
        transaction
            .execute(
                "INSERT INTO runtime_configurations(
                    configuration_hash, system_instructions_hash, repository_instructions_hash,
                    policy_read, policy_write, policy_command, policy_network,
                    policy_destructive, max_duration_ms, max_input_tokens,
                    max_output_tokens, max_tool_calls, max_cost_micros,
                    context_window, context_max_input_tokens,
                    context_reserved_output_tokens, checkpoint_id,
                    input_cost_micros_per_1k, output_cost_micros_per_1k
                 ) VALUES (
                    ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10,
                    ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19
                 )
                 ON CONFLICT(configuration_hash) DO NOTHING",
                params![
                    configuration_hash,
                    system_instructions_hash,
                    repository_instructions_hash,
                    policy_decisions[0],
                    policy_decisions[1],
                    policy_decisions[2],
                    policy_decisions[3],
                    policy_decisions[4],
                    max_duration_ms,
                    max_input_tokens,
                    max_output_tokens,
                    max_tool_calls,
                    max_cost_micros,
                    context_window,
                    context_max_input_tokens,
                    context_reserved_output_tokens,
                    config
                        .checkpoint_id
                        .map(|id| id.as_uuid().as_bytes().to_vec()),
                    input_cost_micros_per_1k,
                    output_cost_micros_per_1k,
                ],
            )
            .map_err(|error| {
                persistence_error(
                    format!("could not save run runtime profile for {run_id}: {error}"),
                    true,
                )
            })?;
        transaction
            .execute(
                "INSERT INTO run_runtime_config(run_id, configuration_hash, context_inspection)
                 VALUES (?1, ?2, ?3)
                 ON CONFLICT(run_id) DO UPDATE SET
                    configuration_hash=excluded.configuration_hash,
                    context_inspection=excluded.context_inspection
                 WHERE run_runtime_config.configuration_hash IS NOT excluded.configuration_hash
                    OR run_runtime_config.context_inspection IS NOT excluded.context_inspection",
                params![
                    run_id.as_uuid().as_bytes().as_slice(),
                    configuration_hash,
                    context_inspection,
                ],
            )
            .map_err(|error| {
                persistence_error(
                    format!("could not attach runtime profile to run {run_id}: {error}"),
                    true,
                )
            })?;
    }
    collect_unused_content(transaction, MAX_CONTENT_GC_CANDIDATES_PER_WRITE)
}

fn save_run_summary_rows(
    transaction: &Transaction<'_>,
    summaries: &BTreeMap<RunId, DurableRunSummary>,
) -> Result<()> {
    for (run_id, summary) in summaries {
        if summary.snapshot.id != *run_id {
            return Err(LoomError::new(
                ErrorCode::MalformedPayload,
                "run summary key does not match its run id",
                false,
            ));
        }
        if summary.snapshot.task.len() > 1024 * 1024
            || summary.snapshot.model.as_str().len() > 256
            || summary
                .snapshot
                .summary
                .as_ref()
                .is_some_and(|summary| summary.len() > 1024 * 1024)
        {
            return Err(LoomError::new(
                ErrorCode::Persistence,
                "run summary exceeds its maximum supported size",
                false,
            ));
        }
        let run_id_bytes = run_id.as_uuid().as_bytes();
        let session_id_bytes = summary.snapshot.session_id.as_uuid().as_bytes();
        transaction
            .execute(
                "INSERT INTO run_summaries(
                    run_id, session_id, attempt_id, control_revision, state, started_at, updated_at,
                    completed_at, task, model, summary,
                    input_tokens, output_tokens, cached_input_tokens, tool_calls, cost_micros, elapsed_ms
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17)
                 ON CONFLICT(run_id) DO UPDATE SET
                    session_id=excluded.session_id,
                    attempt_id=excluded.attempt_id,
                    control_revision=excluded.control_revision,
                    state=excluded.state,
                    started_at=excluded.started_at,
                    updated_at=excluded.updated_at,
                    completed_at=excluded.completed_at,
                    task=excluded.task,
                    model=excluded.model,
                    summary=excluded.summary,
                    input_tokens=excluded.input_tokens,
                    output_tokens=excluded.output_tokens,
                    cached_input_tokens=excluded.cached_input_tokens,
                    tool_calls=excluded.tool_calls,
                    cost_micros=excluded.cost_micros,
                    elapsed_ms=excluded.elapsed_ms
                 WHERE run_summaries.session_id IS NOT excluded.session_id
                    OR run_summaries.attempt_id IS NOT excluded.attempt_id
                    OR run_summaries.control_revision IS NOT excluded.control_revision
                    OR run_summaries.state IS NOT excluded.state
                    OR run_summaries.started_at IS NOT excluded.started_at
                    OR run_summaries.updated_at IS NOT excluded.updated_at
                    OR run_summaries.completed_at IS NOT excluded.completed_at
                    OR run_summaries.task IS NOT excluded.task
                    OR run_summaries.model IS NOT excluded.model
                    OR run_summaries.summary IS NOT excluded.summary
                    OR run_summaries.input_tokens IS NOT excluded.input_tokens
                    OR run_summaries.output_tokens IS NOT excluded.output_tokens
                    OR run_summaries.cached_input_tokens IS NOT excluded.cached_input_tokens
                    OR run_summaries.tool_calls IS NOT excluded.tool_calls
                    OR run_summaries.cost_micros IS NOT excluded.cost_micros
                    OR run_summaries.elapsed_ms IS NOT excluded.elapsed_ms",
                params![
                    run_id_bytes.as_slice(),
                    session_id_bytes.as_slice(),
                    summary.snapshot.attempt_id.as_uuid().as_bytes().as_slice(),
                    encode_counter(summary.snapshot.control_revision, "run control revision")?,
                    run_state_name(summary.snapshot.state),
                    encode_timestamp(summary.snapshot.started_at)?,
                    encode_timestamp(summary.snapshot.updated_at)?,
                    summary
                        .snapshot
                        .completed_at
                        .map(encode_timestamp)
                        .transpose()?,
                    summary.snapshot.task,
                    summary.snapshot.model.as_str(),
                    summary.snapshot.summary,
                    encode_counter(summary.usage.input_tokens, "input token count")?,
                    encode_counter(summary.usage.output_tokens, "output token count")?,
                    encode_counter(summary.usage.cached_input_tokens, "cached input token count")?,
                    encode_counter(summary.usage.tool_calls, "tool call count")?,
                    encode_counter(summary.usage.cost_micros, "cost")?,
                    encode_counter(summary.usage.elapsed_ms, "elapsed time")?,
                ],
            )
            .map_err(|error| {
                persistence_error(
                    format!("could not save run summary {run_id}: {error}"),
                    true,
                )
            })?;
        save_run_evidence_rows(
            transaction,
            *run_id,
            summary.snapshot.session_id,
            &summary.snapshot.evidence,
        )?;
    }
    save_run_interaction_rows(transaction, summaries)?;
    Ok(())
}

fn save_run_evidence_rows(
    transaction: &Transaction<'_>,
    run_id: RunId,
    session_id: AgentSessionId,
    evidence: &[loom_core::EvidenceLink],
) -> Result<()> {
    for (ordinal, link) in evidence.iter().enumerate() {
        if link.label.len() > 16_384 || link.uri.len() > 16_384 {
            return Err(LoomError::new(
                ErrorCode::Persistence,
                "run evidence field exceeds its maximum supported size",
                false,
            ));
        }
        transaction
            .execute(
                "INSERT INTO run_evidence(run_id, session_id, ordinal, label, uri)
                 VALUES (?1, ?2, ?3, ?4, ?5)
                 ON CONFLICT(run_id, ordinal) DO UPDATE SET
                    session_id=excluded.session_id,
                    label=excluded.label,
                    uri=excluded.uri
                 WHERE run_evidence.session_id IS NOT excluded.session_id
                    OR run_evidence.label IS NOT excluded.label
                    OR run_evidence.uri IS NOT excluded.uri",
                params![
                    run_id.as_uuid().as_bytes().as_slice(),
                    session_id.as_uuid().as_bytes().as_slice(),
                    i64::try_from(ordinal).map_err(|_| {
                        LoomError::new(ErrorCode::Persistence, "too many evidence links", false)
                    })?,
                    link.label,
                    link.uri,
                ],
            )
            .map_err(|error| {
                persistence_error(
                    format!("could not save evidence for run {run_id}: {error}"),
                    true,
                )
            })?;
    }
    transaction
        .execute(
            "DELETE FROM run_evidence WHERE run_id=?1 AND ordinal >= ?2",
            params![
                run_id.as_uuid().as_bytes().as_slice(),
                i64::try_from(evidence.len()).map_err(|_| {
                    LoomError::new(ErrorCode::Persistence, "too many evidence links", false)
                })?,
            ],
        )
        .map_err(|error| {
            persistence_error(
                format!("could not prune evidence for run {run_id}: {error}"),
                true,
            )
        })?;
    Ok(())
}

fn load_run_evidence_rows(
    connection: &Connection,
    run_ids: impl Iterator<Item = RunId>,
) -> Result<BTreeMap<RunId, Vec<loom_core::EvidenceLink>>> {
    let run_ids = run_ids.collect::<Vec<_>>();
    let mut evidence = BTreeMap::<RunId, Vec<loom_core::EvidenceLink>>::new();
    for chunk in run_ids.chunks(500) {
        let placeholders = (1..=chunk.len())
            .map(|index| format!("?{index}"))
            .collect::<Vec<_>>()
            .join(",");
        let sql = format!(
            "SELECT run_id, label, uri FROM run_evidence
             WHERE run_id IN ({placeholders}) ORDER BY run_id, ordinal"
        );
        let values = chunk
            .iter()
            .map(|run_id| rusqlite::types::Value::Blob(run_id.as_uuid().as_bytes().to_vec()))
            .collect::<Vec<_>>();
        let mut statement = connection.prepare(&sql).map_err(|error| {
            persistence_error(format!("could not prepare run evidence: {error}"), true)
        })?;
        let rows = statement
            .query_map(rusqlite::params_from_iter(values), |row| {
                Ok((
                    row.get::<_, Vec<u8>>(0)?,
                    loom_core::EvidenceLink {
                        label: row.get(1)?,
                        uri: row.get(2)?,
                    },
                ))
            })
            .map_err(|error| {
                persistence_error(format!("could not read run evidence: {error}"), true)
            })?;
        for row in rows {
            let (run_id, link) = row.map_err(|error| {
                persistence_error(format!("could not read run evidence: {error}"), true)
            })?;
            let run_id = RunId::from_uuid(decode_uuid(&run_id, "evidence run id")?);
            evidence.entry(run_id).or_default().push(link);
        }
    }
    Ok(evidence)
}

fn save_run_context_checkpoint_rows(
    transaction: &Transaction<'_>,
    checkpoints: &BTreeMap<RunId, Option<DurableRunContextCheckpoint>>,
) -> Result<()> {
    for (run_id, checkpoint) in checkpoints {
        let run_id_bytes = run_id.as_uuid().as_bytes();
        let Some(checkpoint) = checkpoint else {
            transaction
                .execute(
                    "DELETE FROM run_context_checkpoints WHERE run_id=?1",
                    [run_id_bytes.as_slice()],
                )
                .map_err(|error| {
                    persistence_error(
                        format!("could not clear run context checkpoint: {error}"),
                        true,
                    )
                })?;
            continue;
        };
        if checkpoint.summary.text.len() > MAX_CONTENT_BYTES
            || checkpoint.summary.source_digest.len() > 64
        {
            return Err(LoomError::new(
                ErrorCode::MalformedPayload,
                "run context checkpoint metadata is invalid",
                false,
            ));
        }
        let session_id: Vec<u8> = transaction
            .query_row(
                "SELECT session_id FROM run_summaries WHERE run_id=?1",
                [run_id_bytes.as_slice()],
                |row| row.get(0),
            )
            .map_err(|error| {
                persistence_error(
                    format!("run {run_id} has no summary for its context checkpoint: {error}"),
                    true,
                )
            })?;
        if session_id.as_slice() != checkpoint.session_id.as_uuid().as_bytes() {
            return Err(LoomError::new(
                ErrorCode::MalformedPayload,
                "run context checkpoint belongs to a different session",
                false,
            ));
        }
        let summary_hash = store_content(transaction, checkpoint.summary.text.as_bytes())?;
        let source_count = encode_counter(
            checkpoint.summary.source_message_count as u64,
            "context message count",
        )?;
        let projection_version = i64::from(checkpoint.summary.projection_version);
        let created_at = encode_timestamp(checkpoint.summary.created_at)?;
        transaction
            .execute(
                "INSERT INTO run_context_checkpoints(
                    run_id, session_id, summary_hash, source_message_count,
                    projection_version, source_digest, created_at
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
                 ON CONFLICT(run_id) DO UPDATE SET
                    session_id=excluded.session_id,
                    summary_hash=excluded.summary_hash,
                    source_message_count=excluded.source_message_count,
                    projection_version=excluded.projection_version,
                    source_digest=excluded.source_digest,
                    created_at=excluded.created_at
                 WHERE run_context_checkpoints.session_id IS NOT excluded.session_id
                    OR run_context_checkpoints.summary_hash IS NOT excluded.summary_hash
                    OR run_context_checkpoints.source_message_count IS NOT excluded.source_message_count
                    OR run_context_checkpoints.projection_version IS NOT excluded.projection_version
                    OR run_context_checkpoints.source_digest IS NOT excluded.source_digest
                    OR run_context_checkpoints.created_at IS NOT excluded.created_at",
                params![
                    run_id_bytes.as_slice(),
                    session_id,
                    summary_hash,
                    source_count,
                    projection_version,
                    checkpoint.summary.source_digest,
                    created_at,
                ],
            )
            .map_err(|error| {
                persistence_error(format!("could not save run context checkpoint: {error}"), true)
            })?;
    }
    collect_unused_content(transaction, MAX_CONTENT_GC_CANDIDATES_PER_WRITE)?;
    Ok(())
}

fn save_run_plan_rows(
    transaction: &Transaction<'_>,
    plans: &BTreeMap<RunId, AgentPlan>,
    summaries: Option<&BTreeMap<RunId, DurableRunSummary>>,
) -> Result<()> {
    for (run_id, plan) in plans {
        let session_id = summaries
            .and_then(|summaries| summaries.get(run_id))
            .map(|summary| summary.snapshot.session_id)
            .ok_or_else(|| {
                LoomError::new(
                    ErrorCode::MalformedPayload,
                    format!("run plan {run_id} has no matching run summary"),
                    false,
                )
            })?;
        for (ordinal, step) in plan.steps.iter().enumerate() {
            if step.id.len() > 256 || step.description.len() > 16_384 {
                return Err(LoomError::new(
                    ErrorCode::Persistence,
                    "run plan step exceeds its maximum supported size",
                    false,
                ));
            }
            transaction
                .execute(
                    "INSERT INTO run_plan_steps(run_id, session_id, ordinal, step_id, description)
                     VALUES (?1, ?2, ?3, ?4, ?5)
                     ON CONFLICT(run_id, ordinal) DO UPDATE SET
                        session_id=excluded.session_id,
                        step_id=excluded.step_id,
                        description=excluded.description
                     WHERE run_plan_steps.session_id IS NOT excluded.session_id
                        OR run_plan_steps.step_id IS NOT excluded.step_id
                        OR run_plan_steps.description IS NOT excluded.description",
                    params![
                        run_id.as_uuid().as_bytes().as_slice(),
                        session_id.as_uuid().as_bytes().as_slice(),
                        i64::try_from(ordinal).map_err(|_| {
                            LoomError::new(ErrorCode::Persistence, "too many plan steps", false)
                        })?,
                        step.id,
                        step.description,
                    ],
                )
                .map_err(|error| {
                    persistence_error(
                        format!("could not save plan for run {run_id}: {error}"),
                        true,
                    )
                })?;
        }
        transaction
            .execute(
                "DELETE FROM run_plan_steps WHERE run_id=?1 AND ordinal >= ?2",
                params![
                    run_id.as_uuid().as_bytes().as_slice(),
                    i64::try_from(plan.steps.len()).map_err(|_| {
                        LoomError::new(ErrorCode::Persistence, "too many plan steps", false)
                    })?,
                ],
            )
            .map_err(|error| {
                persistence_error(
                    format!("could not prune plan for run {run_id}: {error}"),
                    true,
                )
            })?;
    }
    Ok(())
}

fn save_run_execution_state_rows(
    transaction: &Transaction<'_>,
    summaries: &BTreeMap<RunId, DurableRunSummary>,
) -> Result<()> {
    for (run_id, summary) in summaries {
        let Some(execution) = summary.execution_state.as_ref() else {
            continue;
        };
        if execution.run_id != *run_id
            || execution.session_id != summary.snapshot.session_id
            || execution.attempt_id != summary.snapshot.attempt_id
            || execution.control_revision != summary.snapshot.control_revision
            || execution.state != summary.snapshot.state
        {
            return Err(LoomError::new(
                ErrorCode::MalformedPayload,
                "run execution identity and revision do not match its summary",
                false,
            ));
        }
        if execution
            .pending_input
            .as_ref()
            .is_some_and(|input| input.len() > 65_536)
            || (execution.pending_tool_execution.is_some()
                && !matches!(
                    execution.state,
                    AgentRunState::Executing | AgentRunState::Evaluating
                ))
            || (execution.pending_approval.is_some()
                && !matches!(
                    execution.state,
                    AgentRunState::AwaitingApproval | AgentRunState::Paused
                ))
            || (execution.pending_input.is_some()
                && !matches!(
                    execution.state,
                    AgentRunState::NeedsInput | AgentRunState::Paused
                ))
        {
            return Err(LoomError::new(
                ErrorCode::MalformedPayload,
                format!(
                    "pending execution intent does not match run state {:?} (tool={}, approval={}, input={})",
                    execution.state,
                    execution.pending_tool_execution.is_some(),
                    execution.pending_approval.is_some(),
                    execution.pending_input.is_some()
                ),
                false,
            ));
        }
        let encode_tool_call = |call: &Option<loom_model::ToolCall>| -> Result<Option<String>> {
            call.as_ref()
                .map(|call| {
                    serde_json::to_string(call).map_err(|error| {
                        persistence_error(
                            format!("could not encode execution tool call: {error}"),
                            false,
                        )
                    })
                })
                .transpose()
        };
        let pending_tool_execution = encode_tool_call(&execution.pending_tool_execution)?;
        let pending_approval = encode_tool_call(&execution.pending_approval)?;
        let last_failed_call = encode_tool_call(&execution.last_failed_call)?;
        if [
            pending_tool_execution.as_ref(),
            pending_approval.as_ref(),
            last_failed_call.as_ref(),
        ]
        .into_iter()
        .flatten()
        .any(|call| call.len() > 1_048_576)
        {
            return Err(LoomError::new(
                ErrorCode::Persistence,
                "execution tool call exceeds the maximum supported size",
                false,
            ));
        }
        let control_revision = i64::try_from(execution.control_revision).map_err(|_| {
            LoomError::new(
                ErrorCode::Persistence,
                "execution control revision is out of range",
                false,
            )
        })?;
        let step_index = i64::from(execution.step_index);
        let provider_cursor = i64::try_from(execution.provider_cursor).map_err(|_| {
            LoomError::new(
                ErrorCode::Persistence,
                "provider cursor is out of range",
                false,
            )
        })?;
        let next_message_id = i64::try_from(execution.next_message_id).map_err(|_| {
            LoomError::new(
                ErrorCode::Persistence,
                "next message id is out of range",
                false,
            )
        })?;
        let active_message_id = execution
            .active_message_id
            .map(i64::try_from)
            .transpose()
            .map_err(|_| {
                LoomError::new(
                    ErrorCode::Persistence,
                    "active message id is out of range",
                    false,
                )
            })?;
        let step_id = execution.step_id.map(|id| id.as_uuid().as_bytes().to_vec());
        transaction
            .execute(
                "INSERT INTO run_execution_state(
                    run_id, session_id, attempt_id, control_revision, state, step_id, step_index,
                    provider_cursor, next_message_id, active_message_id, pending_tool_execution,
                    pending_approval, pending_input, last_failed_call
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)
                 ON CONFLICT(run_id) DO UPDATE SET
                    session_id=excluded.session_id,
                    attempt_id=excluded.attempt_id,
                    control_revision=excluded.control_revision,
                    state=excluded.state,
                    step_id=excluded.step_id,
                    step_index=excluded.step_index,
                    provider_cursor=excluded.provider_cursor,
                    next_message_id=excluded.next_message_id,
                    active_message_id=excluded.active_message_id,
                    pending_tool_execution=excluded.pending_tool_execution,
                    pending_approval=excluded.pending_approval,
                    pending_input=excluded.pending_input,
                    last_failed_call=excluded.last_failed_call
                 WHERE run_execution_state.session_id IS NOT excluded.session_id
                    OR run_execution_state.attempt_id IS NOT excluded.attempt_id
                    OR run_execution_state.control_revision IS NOT excluded.control_revision
                    OR run_execution_state.state IS NOT excluded.state
                    OR run_execution_state.step_id IS NOT excluded.step_id
                    OR run_execution_state.step_index IS NOT excluded.step_index
                    OR run_execution_state.provider_cursor IS NOT excluded.provider_cursor
                    OR run_execution_state.next_message_id IS NOT excluded.next_message_id
                    OR run_execution_state.active_message_id IS NOT excluded.active_message_id
                    OR run_execution_state.pending_tool_execution IS NOT excluded.pending_tool_execution
                    OR run_execution_state.pending_approval IS NOT excluded.pending_approval
                    OR run_execution_state.pending_input IS NOT excluded.pending_input
                    OR run_execution_state.last_failed_call IS NOT excluded.last_failed_call",
                params![
                    run_id.as_uuid().as_bytes().as_slice(),
                    execution.session_id.as_uuid().as_bytes().as_slice(),
                    execution.attempt_id.as_uuid().as_bytes().as_slice(),
                    control_revision,
                    run_state_name(execution.state),
                    step_id,
                    step_index,
                    provider_cursor,
                    next_message_id,
                    active_message_id,
                    pending_tool_execution,
                    pending_approval,
                    execution.pending_input,
                    last_failed_call,
                ],
            )
            .map_err(|error| {
                persistence_error(
                    format!("could not save run execution state for {run_id}: {error}"),
                    true,
                )
            })?;
    }
    Ok(())
}

fn save_run_attempt_rows(
    transaction: &Transaction<'_>,
    summaries: &BTreeMap<RunId, DurableRunSummary>,
) -> Result<()> {
    transaction
        .execute_batch(
            "CREATE TEMP TABLE IF NOT EXISTS _loom_wanted_run_attempts (
                run_id BLOB NOT NULL, attempt_id BLOB NOT NULL,
                PRIMARY KEY(run_id, attempt_id)
             ) WITHOUT ROWID, STRICT;",
        )
        .map_err(|error| {
            persistence_error(format!("could not stage run attempts: {error}"), true)
        })?;
    for (run_id, summary) in summaries {
        let Some(attempts) = summary.attempts.as_ref() else {
            continue;
        };
        let session_id = summary.snapshot.session_id;
        let current_attempt = attempts.iter().find(|attempt| {
            attempt.id == summary.snapshot.attempt_id
                && attempt.run_id == *run_id
                && attempt.session_id == session_id
                && attempt.state == summary.snapshot.state
        });
        let latest_number = attempts.iter().map(|attempt| attempt.number).max();
        if current_attempt.is_none()
            || current_attempt.map(|attempt| attempt.number) != latest_number
        {
            return Err(LoomError::new(
                ErrorCode::MalformedPayload,
                "run attempt history does not contain the current attempt as its latest record",
                false,
            ));
        }
        let run_id_bytes = run_id.as_uuid().as_bytes();
        transaction
            .execute(
                "DELETE FROM _loom_wanted_run_attempts WHERE run_id=?1",
                [run_id_bytes.as_slice()],
            )
            .map_err(|error| {
                persistence_error(
                    format!("could not reset staged attempts for run {run_id}: {error}"),
                    true,
                )
            })?;
        let mut seen_ids = BTreeSet::new();
        let mut seen_numbers = BTreeSet::new();
        for attempt in attempts {
            if attempt.run_id != *run_id
                || attempt.session_id != session_id
                || attempt.number == 0
                || !seen_ids.insert(attempt.id)
                || !seen_numbers.insert(attempt.number)
                || attempt
                    .completed_at
                    .is_some_and(|completed| completed < attempt.started_at)
                || (matches!(
                    attempt.state,
                    AgentRunState::Completed | AgentRunState::Failed | AgentRunState::Cancelled
                ) != attempt.completed_at.is_some())
            {
                return Err(LoomError::new(
                    ErrorCode::MalformedPayload,
                    "run attempt ownership, identity, state, or timestamps are invalid",
                    false,
                ));
            }
            let attempt_id = attempt.id.as_uuid().as_bytes();
            let attempt_number = i64::from(attempt.number);
            let checkpoint_id = attempt
                .checkpoint_id
                .map(|id| id.as_uuid().as_bytes().to_vec());
            transaction
                .execute(
                    "INSERT INTO _loom_wanted_run_attempts(run_id, attempt_id)
                     VALUES (?1, ?2)",
                    params![run_id_bytes.as_slice(), attempt_id.as_slice()],
                )
                .map_err(|error| {
                    persistence_error(
                        format!("could not stage run attempt {}: {error}", attempt.id),
                        true,
                    )
                })?;
            transaction
                .execute(
                    "INSERT INTO run_attempts(
                        run_id, session_id, attempt_id, attempt_number, state,
                        checkpoint_id, started_at, completed_at
                     ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
                     ON CONFLICT(run_id, attempt_id) DO UPDATE SET
                        session_id=excluded.session_id,
                        attempt_number=excluded.attempt_number,
                        state=excluded.state,
                        checkpoint_id=excluded.checkpoint_id,
                        started_at=excluded.started_at,
                        completed_at=excluded.completed_at
                     WHERE run_attempts.session_id IS NOT excluded.session_id
                        OR run_attempts.attempt_number IS NOT excluded.attempt_number
                        OR run_attempts.state IS NOT excluded.state
                        OR run_attempts.checkpoint_id IS NOT excluded.checkpoint_id
                        OR run_attempts.started_at IS NOT excluded.started_at
                        OR run_attempts.completed_at IS NOT excluded.completed_at",
                    params![
                        run_id_bytes.as_slice(),
                        session_id.as_uuid().as_bytes().as_slice(),
                        attempt_id.as_slice(),
                        attempt_number,
                        run_state_name(attempt.state),
                        checkpoint_id,
                        encode_timestamp(attempt.started_at)?,
                        attempt.completed_at.map(encode_timestamp).transpose()?,
                    ],
                )
                .map_err(|error| {
                    persistence_error(
                        format!("could not save run attempt {}: {error}", attempt.id),
                        true,
                    )
                })?;
        }
        transaction
            .execute(
                "DELETE FROM run_attempts
                 WHERE run_id=?1 AND NOT EXISTS (
                    SELECT 1 FROM _loom_wanted_run_attempts wanted
                    WHERE wanted.run_id=run_attempts.run_id
                      AND wanted.attempt_id=run_attempts.attempt_id
                 )",
                [run_id_bytes.as_slice()],
            )
            .map_err(|error| {
                persistence_error(
                    format!("could not prune run attempts for {run_id}: {error}"),
                    true,
                )
            })?;
    }
    Ok(())
}

fn save_run_interaction_rows(
    transaction: &Transaction<'_>,
    summaries: &BTreeMap<RunId, DurableRunSummary>,
) -> Result<()> {
    transaction
        .execute_batch(
            "CREATE TEMP TABLE IF NOT EXISTS _loom_wanted_run_interactions (
                interaction_id BLOB PRIMARY KEY NOT NULL
             ) WITHOUT ROWID, STRICT;",
        )
        .map_err(|error| {
            persistence_error(format!("could not stage run interactions: {error}"), true)
        })?;
    for (run_id, summary) in summaries {
        let Some(interactions) = summary.interactions.as_ref() else {
            continue;
        };
        let run_id_bytes = run_id.as_uuid().as_bytes();
        let session_id = summary.snapshot.session_id;
        transaction
            .execute("DELETE FROM _loom_wanted_run_interactions", [])
            .map_err(|error| {
                persistence_error(
                    format!("could not reset staged interactions: {error}"),
                    true,
                )
            })?;
        let mut seen_ids = BTreeSet::new();
        let mut seen_revisions = BTreeSet::new();
        for interaction in interactions {
            if interaction.run_id != *run_id
                || interaction.session_id != session_id
                || !seen_ids.insert(interaction.id)
                || !seen_revisions.insert((interaction.attempt_id, interaction.control_revision))
                || interaction.prompt.len() > 65_536
            {
                return Err(LoomError::new(
                    ErrorCode::MalformedPayload,
                    "run interaction ownership, identity, revision, or prompt is invalid",
                    false,
                ));
            }
            let interaction_id = interaction.id.as_uuid().as_bytes();
            let attempt_id = interaction.attempt_id.as_uuid().as_bytes();
            let tool_call_id = interaction
                .tool_call_id
                .map(|id| id.as_uuid().as_bytes().to_vec());
            let control_revision = i64::try_from(interaction.control_revision).map_err(|_| {
                LoomError::new(
                    ErrorCode::Persistence,
                    "run interaction revision is out of range",
                    false,
                )
            })?;
            let decision = interaction.decision.map(approval_decision_name);
            transaction
                .execute(
                    "INSERT INTO _loom_wanted_run_interactions(interaction_id) VALUES (?1)",
                    [interaction_id.as_slice()],
                )
                .map_err(|error| {
                    persistence_error(
                        format!("could not stage interaction {}: {error}", interaction.id),
                        true,
                    )
                })?;
            transaction
                .execute(
                    "INSERT INTO run_interactions(
                        run_id, session_id, interaction_id, attempt_id, control_revision,
                        kind, status, tool_call_id, prompt, decision, created_at, resolved_at
                     ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)
                     ON CONFLICT(run_id, interaction_id) DO UPDATE SET
                        session_id=excluded.session_id,
                        attempt_id=excluded.attempt_id,
                        control_revision=excluded.control_revision,
                        kind=excluded.kind,
                        status=excluded.status,
                        tool_call_id=excluded.tool_call_id,
                        prompt=excluded.prompt,
                        decision=excluded.decision,
                        created_at=excluded.created_at,
                        resolved_at=excluded.resolved_at
                     WHERE run_interactions.session_id IS NOT excluded.session_id
                        OR run_interactions.attempt_id IS NOT excluded.attempt_id
                        OR run_interactions.control_revision IS NOT excluded.control_revision
                        OR run_interactions.kind IS NOT excluded.kind
                        OR run_interactions.status IS NOT excluded.status
                        OR run_interactions.tool_call_id IS NOT excluded.tool_call_id
                        OR run_interactions.prompt IS NOT excluded.prompt
                        OR run_interactions.decision IS NOT excluded.decision
                        OR run_interactions.created_at IS NOT excluded.created_at
                        OR run_interactions.resolved_at IS NOT excluded.resolved_at",
                    params![
                        run_id_bytes.as_slice(),
                        session_id.as_uuid().as_bytes().as_slice(),
                        interaction_id.as_slice(),
                        attempt_id.as_slice(),
                        control_revision,
                        interaction_kind_name(interaction.kind),
                        interaction_status_name(interaction.status),
                        tool_call_id,
                        interaction.prompt.as_str(),
                        decision,
                        encode_timestamp(interaction.created_at)?,
                        interaction.resolved_at.map(encode_timestamp).transpose()?,
                    ],
                )
                .map_err(|error| {
                    persistence_error(
                        format!("could not save interaction {}: {error}", interaction.id),
                        true,
                    )
                })?;
        }
        transaction
            .execute(
                "DELETE FROM run_interactions
                 WHERE run_id=?1 AND NOT EXISTS (
                    SELECT 1 FROM _loom_wanted_run_interactions wanted
                    WHERE wanted.interaction_id=run_interactions.interaction_id
                 )",
                [run_id_bytes.as_slice()],
            )
            .map_err(|error| {
                persistence_error(
                    format!("could not prune run interactions for {run_id}: {error}"),
                    true,
                )
            })?;
    }
    Ok(())
}

fn save_run_activity_rows(
    transaction: &Transaction<'_>,
    activities_by_run: &DurableRunActivities,
) -> Result<()> {
    transaction
        .execute_batch(
            "CREATE TEMP TABLE IF NOT EXISTS _loom_wanted_run_activities (
                run_id BLOB NOT NULL, activity_id BLOB NOT NULL,
                PRIMARY KEY(run_id, activity_id)
             ) WITHOUT ROWID, STRICT;
             DELETE FROM _loom_wanted_run_activities;",
        )
        .map_err(|error| {
            persistence_error(format!("could not stage run activities: {error}"), true)
        })?;
    for (run_id, activities) in activities_by_run {
        let run_id_bytes = run_id.as_uuid().as_bytes();
        let session_id: Vec<u8> = transaction
            .query_row(
                "SELECT session_id FROM run_summaries WHERE run_id=?1",
                [run_id_bytes.as_slice()],
                |row| row.get(0),
            )
            .map_err(|error| {
                persistence_error(
                    format!("run {run_id} has no durable summary for its activities: {error}"),
                    true,
                )
            })?;
        let mut seen = BTreeSet::new();
        for (ordinal, activity) in activities.iter().enumerate() {
            if activity.run_id != *run_id || !seen.insert(activity.id) {
                return Err(LoomError::invalid_request(
                    "run activity keys must be unique and match their owning run",
                ));
            }
            if activity.kind != activity_data_kind(&activity.data) {
                return Err(LoomError::invalid_request(
                    "run activity kind does not match its data",
                ));
            }
            let ordinal = i64::try_from(ordinal).map_err(|_| {
                LoomError::new(
                    ErrorCode::Persistence,
                    "run has too many activity records",
                    false,
                )
            })?;
            let activity_id = activity.id.as_uuid().as_bytes().to_vec();
            let existing_ordinal: Option<i64> = transaction
                .query_row(
                    "SELECT ordinal FROM run_activities
                     WHERE run_id=?1 AND activity_id=?2",
                    params![run_id_bytes.as_slice(), activity_id.as_slice()],
                    |row| row.get(0),
                )
                .optional()
                .map_err(|error| {
                    persistence_error(format!("could not read activity position: {error}"), true)
                })?;
            if existing_ordinal.is_some_and(|existing| existing != ordinal) {
                return Err(LoomError::new(
                    ErrorCode::MalformedPayload,
                    "persisted activity order cannot be changed",
                    false,
                ));
            }
            let occupant: Option<Vec<u8>> = transaction
                .query_row(
                    "SELECT activity_id FROM run_activities WHERE run_id=?1 AND ordinal=?2",
                    params![run_id_bytes.as_slice(), ordinal],
                    |row| row.get(0),
                )
                .optional()
                .map_err(|error| {
                    persistence_error(format!("could not verify activity order: {error}"), true)
                })?;
            if occupant
                .as_ref()
                .is_some_and(|existing| existing.as_slice() != activity_id.as_slice())
            {
                return Err(LoomError::new(
                    ErrorCode::MalformedPayload,
                    "persisted activity order cannot be changed",
                    false,
                ));
            }

            let normalized_data = normalize_activity_tool_data(&activity.data);
            let data = serde_json::to_vec(&normalized_data).map_err(|error| {
                LoomError::new(
                    ErrorCode::Internal,
                    format!("could not encode run activity data: {error}"),
                    false,
                )
            })?;
            let data_hash = store_content(transaction, &data)?;
            let parent_activity_id = activity
                .parent_id
                .map(|id| id.as_uuid().as_bytes().to_vec());
            let step_id = activity.step_id.map(|id| id.as_uuid().as_bytes().to_vec());
            let tool_call_id = activity_data_tool_call_id(&activity.data)
                .map(|id| id.as_uuid().as_bytes().to_vec());
            let started_at = i64::try_from(activity.started_at.as_unix_millis())
                .map_err(|_| LoomError::invalid_request("activity start time is out of range"))?;
            let completed_at = activity
                .completed_at
                .map(|time| {
                    i64::try_from(time.as_unix_millis()).map_err(|_| {
                        LoomError::invalid_request("activity end time is out of range")
                    })
                })
                .transpose()?;
            let elapsed_ms = activity
                .elapsed_ms
                .map(|elapsed| {
                    i64::try_from(elapsed).map_err(|_| {
                        LoomError::invalid_request("activity duration is out of range")
                    })
                })
                .transpose()?;
            transaction
                .execute(
                    "INSERT INTO run_activities(
                        run_id, session_id, activity_id, ordinal, parent_activity_id,
                        step_id, tool_call_id, kind, status, started_at, completed_at,
                        elapsed_ms, data_hash
                     ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)
                     ON CONFLICT(run_id, activity_id) DO UPDATE SET
                        session_id=excluded.session_id,
                        ordinal=excluded.ordinal,
                        parent_activity_id=excluded.parent_activity_id,
                        step_id=excluded.step_id,
                        tool_call_id=excluded.tool_call_id,
                        kind=excluded.kind,
                        status=excluded.status,
                        started_at=excluded.started_at,
                        completed_at=excluded.completed_at,
                        elapsed_ms=excluded.elapsed_ms,
                        data_hash=excluded.data_hash
                     WHERE run_activities.session_id IS NOT excluded.session_id
                        OR run_activities.ordinal IS NOT excluded.ordinal
                        OR run_activities.parent_activity_id IS NOT excluded.parent_activity_id
                        OR run_activities.step_id IS NOT excluded.step_id
                        OR run_activities.tool_call_id IS NOT excluded.tool_call_id
                        OR run_activities.kind IS NOT excluded.kind
                        OR run_activities.status IS NOT excluded.status
                        OR run_activities.started_at IS NOT excluded.started_at
                        OR run_activities.completed_at IS NOT excluded.completed_at
                        OR run_activities.elapsed_ms IS NOT excluded.elapsed_ms
                        OR run_activities.data_hash IS NOT excluded.data_hash",
                    params![
                        run_id_bytes.as_slice(),
                        session_id.as_slice(),
                        activity_id.as_slice(),
                        ordinal,
                        parent_activity_id.as_deref(),
                        step_id.as_deref(),
                        tool_call_id.as_deref(),
                        activity_kind_name(activity.kind),
                        activity_status_name(activity.status),
                        started_at,
                        completed_at,
                        elapsed_ms,
                        data_hash
                    ],
                )
                .map_err(|error| {
                    persistence_error(format!("could not save run activity: {error}"), true)
                })?;
            transaction
                .execute(
                    "INSERT INTO _loom_wanted_run_activities(run_id, activity_id)
                     VALUES (?1, ?2)",
                    params![run_id_bytes.as_slice(), activity_id.as_slice()],
                )
                .map_err(|error| {
                    persistence_error(format!("could not stage run activity: {error}"), true)
                })?;
        }
        transaction
            .execute(
                "DELETE FROM run_activities
                 WHERE run_id=?1 AND NOT EXISTS (
                    SELECT 1 FROM _loom_wanted_run_activities wanted
                    WHERE wanted.run_id=run_activities.run_id
                      AND wanted.activity_id=run_activities.activity_id
                 )",
                [run_id_bytes.as_slice()],
            )
            .map_err(|error| {
                persistence_error(format!("could not prune run activities: {error}"), true)
            })?;
    }
    collect_unused_content(transaction, MAX_CONTENT_GC_CANDIDATES_PER_WRITE)
}

/// Persist only worker-reported activity changes. Existing activity ordinals
/// are recovered by stable ID; new activities receive the next ordinal. This
/// path deliberately does not stage or prune the run's complete activity set.
fn save_run_activity_deltas(
    transaction: &Transaction<'_>,
    run_id: RunId,
    activities: &[AgentActivityRecord],
    summaries: &BTreeMap<RunId, DurableRunSummary>,
) -> Result<()> {
    let run_id_bytes = run_id.as_uuid().as_bytes();
    let session_id: Vec<u8> = transaction
        .query_row(
            "SELECT session_id FROM run_summaries WHERE run_id=?1",
            [run_id_bytes.as_slice()],
            |row| row.get(0),
        )
        .map_err(|error| {
            persistence_error(
                format!("run {run_id} has no durable summary for activity changes: {error}"),
                true,
            )
        })?;
    let mut seen = BTreeSet::new();
    for activity in activities {
        if activity.run_id != run_id || !seen.insert(activity.id) {
            return Err(LoomError::invalid_request(
                "activity deltas must have unique IDs matching their run",
            ));
        }
        if activity.kind != activity_data_kind(&activity.data) {
            return Err(LoomError::invalid_request(
                "run activity kind does not match its data",
            ));
        }
        let activity_id = activity.id.as_uuid().as_bytes();
        let existing: Option<i64> = transaction
            .query_row(
                "SELECT ordinal FROM run_activities WHERE run_id=?1 AND activity_id=?2",
                params![run_id_bytes.as_slice(), activity_id.as_slice()],
                |row| row.get(0),
            )
            .optional()
            .map_err(|error| {
                persistence_error(format!("could not read activity ordinal: {error}"), true)
            })?;
        let ordinal = if let Some(ordinal) = existing {
            ordinal
        } else {
            transaction
                .query_row(
                    "SELECT COALESCE(MAX(ordinal) + 1, 0) FROM run_activities WHERE run_id=?1",
                    [run_id_bytes.as_slice()],
                    |row| row.get::<_, i64>(0),
                )
                .map_err(|error| {
                    persistence_error(
                        format!("could not allocate activity ordinal: {error}"),
                        true,
                    )
                })?
        };
        let normalized_data = normalize_activity_tool_data(&activity.data);
        let data = serde_json::to_vec(&normalized_data).map_err(|error| {
            LoomError::new(
                ErrorCode::Internal,
                format!("could not encode run activity data: {error}"),
                false,
            )
        })?;
        let data_hash = store_content(transaction, &data)?;
        let parent_activity_id = activity
            .parent_id
            .map(|id| id.as_uuid().as_bytes().to_vec());
        let step_id = activity.step_id.map(|id| id.as_uuid().as_bytes().to_vec());
        let tool_call_id =
            activity_data_tool_call_id(&activity.data).map(|id| id.as_uuid().as_bytes().to_vec());
        let started_at = i64::try_from(activity.started_at.as_unix_millis())
            .map_err(|_| LoomError::invalid_request("activity start time is out of range"))?;
        let completed_at = activity
            .completed_at
            .map(|time| {
                i64::try_from(time.as_unix_millis())
                    .map_err(|_| LoomError::invalid_request("activity end time is out of range"))
            })
            .transpose()?;
        let elapsed_ms = activity
            .elapsed_ms
            .map(|elapsed| {
                i64::try_from(elapsed)
                    .map_err(|_| LoomError::invalid_request("activity duration is out of range"))
            })
            .transpose()?;
        transaction.execute(
            "INSERT INTO run_activities(
                run_id, session_id, activity_id, ordinal, parent_activity_id,
                step_id, tool_call_id, kind, status, started_at, completed_at,
                elapsed_ms, data_hash
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)
             ON CONFLICT(run_id, activity_id) DO UPDATE SET
                parent_activity_id=excluded.parent_activity_id, step_id=excluded.step_id,
                tool_call_id=excluded.tool_call_id, kind=excluded.kind, status=excluded.status,
                started_at=excluded.started_at, completed_at=excluded.completed_at,
                elapsed_ms=excluded.elapsed_ms, data_hash=excluded.data_hash
             WHERE run_activities.parent_activity_id IS NOT excluded.parent_activity_id
                OR run_activities.step_id IS NOT excluded.step_id
                OR run_activities.tool_call_id IS NOT excluded.tool_call_id
                OR run_activities.kind IS NOT excluded.kind OR run_activities.status IS NOT excluded.status
                OR run_activities.started_at IS NOT excluded.started_at
                OR run_activities.completed_at IS NOT excluded.completed_at
                OR run_activities.elapsed_ms IS NOT excluded.elapsed_ms
                OR run_activities.data_hash IS NOT excluded.data_hash",
            params![run_id_bytes.as_slice(), session_id.as_slice(), activity_id, ordinal,
                parent_activity_id.as_deref(), step_id.as_deref(), tool_call_id.as_deref(),
                activity_kind_name(activity.kind), activity_status_name(activity.status),
                started_at, completed_at, elapsed_ms, data_hash],
        ).map_err(|error| persistence_error(format!("could not save activity delta: {error}"), true))?;
        if let Some((call, result)) = activity_tool_data(&activity.data) {
            save_run_tool_activity_delta(
                transaction,
                run_id,
                &session_id,
                activity,
                call,
                result,
                summaries
                    .get(&run_id)
                    .and_then(|summary| summary.execution_state.as_ref()),
            )?;
        }
    }
    collect_unused_content(transaction, MAX_CONTENT_GC_CANDIDATES_PER_WRITE)
}

fn save_run_tool_activity_delta(
    transaction: &Transaction<'_>,
    run_id: RunId,
    session_id: &[u8],
    activity: &AgentActivityRecord,
    call: &loom_model::ToolCall,
    result: Option<&ToolResult>,
    execution: Option<&AgentExecutionStateRecord>,
) -> Result<()> {
    let run_id_bytes = run_id.as_uuid().as_bytes();
    let arguments = serde_json::to_vec(&call.arguments).map_err(|error| {
        LoomError::new(
            ErrorCode::MalformedPayload,
            format!("could not encode tool-call arguments: {error}"),
            false,
        )
    })?;
    if call.name.len() > 4096 || arguments.len() > MAX_TOOL_ARGUMENT_BYTES {
        return Err(LoomError::invalid_request(
            "tool-call data exceeds the maximum supported size",
        ));
    }
    let arguments_hash = store_content(transaction, &arguments)?;
    let created_at = encode_timestamp(activity.started_at)?;
    let call_id = call.id.as_uuid().as_bytes();
    let existing: Option<(Vec<u8>, String, Vec<u8>)> = transaction.query_row(
        "SELECT session_id, name, arguments_hash FROM run_tool_calls WHERE run_id=?1 AND tool_call_id=?2",
        params![run_id_bytes.as_slice(), call_id.as_slice()],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    ).optional().map_err(|error| persistence_error(format!("could not read logical tool call: {error}"), true))?;
    if let Some((stored_session, stored_name, stored_hash)) = existing {
        if stored_session != session_id || stored_name != call.name || stored_hash != arguments_hash
        {
            return Err(LoomError::new(
                ErrorCode::MalformedPayload,
                "persisted logical tool call is immutable",
                false,
            ));
        }
    } else {
        transaction.execute(
            "INSERT INTO run_tool_calls(run_id,session_id,tool_call_id,name,arguments_hash,created_at)
             VALUES (?1,?2,?3,?4,?5,?6)",
            params![run_id_bytes.as_slice(), session_id, call_id.as_slice(), call.name, arguments_hash, created_at],
        ).map_err(|error| persistence_error(format!("could not save logical tool call: {error}"), true))?;
    }
    let activity_id = activity.id.as_uuid().as_bytes();
    let attempt_number: i64 = if let Some(existing) = transaction
        .query_row(
            "SELECT attempt_number FROM run_tool_attempts WHERE run_id=?1 AND activity_id=?2",
            params![run_id_bytes.as_slice(), activity_id.as_slice()],
            |row| row.get(0),
        )
        .optional()
        .map_err(|error| {
            persistence_error(format!("could not read tool attempt number: {error}"), true)
        })? {
        existing
    } else {
        transaction.query_row(
            "SELECT COALESCE(MAX(attempt_number), 0) + 1 FROM run_tool_attempts WHERE run_id=?1 AND tool_call_id=?2",
            params![run_id_bytes.as_slice(), call_id.as_slice()], |row| row.get(0),
        ).map_err(|error| persistence_error(format!("could not allocate tool attempt number: {error}"), true))?
    };
    let state = match activity.status {
        AgentActivityStatus::Started
            if execution
                .and_then(|e| e.pending_tool_execution.as_ref())
                .is_some_and(|pending| pending.id == call.id) =>
        {
            AgentToolAttemptState::Queued
        }
        AgentActivityStatus::Started
            if execution
                .and_then(|e| e.last_failed_call.as_ref())
                .is_some_and(|failed| failed.id == call.id) =>
        {
            AgentToolAttemptState::OutcomeUnknown
        }
        AgentActivityStatus::Started => AgentToolAttemptState::Running,
        AgentActivityStatus::Completed => AgentToolAttemptState::Completed,
        AgentActivityStatus::Failed => AgentToolAttemptState::Failed,
        AgentActivityStatus::AwaitingApproval => AgentToolAttemptState::AwaitingApproval,
        AgentActivityStatus::AwaitingInput => AgentToolAttemptState::AwaitingInput,
        AgentActivityStatus::Cancelled => AgentToolAttemptState::Cancelled,
    };
    let result_hash = result
        .map(|result| {
            let encoded = serde_json::to_vec(result).map_err(|error| {
                LoomError::new(
                    ErrorCode::MalformedPayload,
                    format!("could not encode tool result: {error}"),
                    false,
                )
            })?;
            store_content(transaction, &encoded)
        })
        .transpose()?;
    let started_at = encode_timestamp(activity.started_at)?;
    let completed_at = activity.completed_at.map(encode_timestamp).transpose()?;
    transaction.execute(
        "INSERT INTO run_tool_attempts(run_id,session_id,activity_id,tool_call_id,attempt_number,state,started_at,completed_at,result_hash)
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)
         ON CONFLICT(run_id,activity_id) DO UPDATE SET state=excluded.state,
            completed_at=excluded.completed_at,result_hash=excluded.result_hash
         WHERE run_tool_attempts.state IS NOT excluded.state
            OR run_tool_attempts.completed_at IS NOT excluded.completed_at
            OR run_tool_attempts.result_hash IS NOT excluded.result_hash",
        params![run_id_bytes.as_slice(), session_id, activity_id, call_id.as_slice(), attempt_number,
            tool_attempt_state_name(state), started_at, completed_at, result_hash],
    ).map_err(|error| persistence_error(format!("could not save tool attempt delta: {error}"), true))?;
    Ok(())
}

fn activity_tool_data(
    data: &AgentActivityData,
) -> Option<(&loom_model::ToolCall, Option<&ToolResult>)> {
    match data {
        AgentActivityData::ModelTurn { .. } => None,
        AgentActivityData::ToolCall { call, result }
        | AgentActivityData::File { call, result, .. }
        | AgentActivityData::Search { call, result, .. }
        | AgentActivityData::Command { call, result, .. } => Some((call, result.as_ref())),
    }
}

fn normalize_activity_tool_data(data: &AgentActivityData) -> AgentActivityData {
    let normalize_call = |call: &loom_model::ToolCall| loom_model::ToolCall {
        id: call.id,
        name: call.name.clone(),
        arguments: Value::Null,
    };
    match data {
        AgentActivityData::ModelTurn { model } => AgentActivityData::ModelTurn {
            model: model.clone(),
        },
        AgentActivityData::ToolCall { call, .. } => AgentActivityData::ToolCall {
            call: normalize_call(call),
            result: None,
        },
        AgentActivityData::File {
            call,
            operation,
            path,
            ..
        } => AgentActivityData::File {
            call: normalize_call(call),
            operation: *operation,
            path: path.clone(),
            result: None,
        },
        AgentActivityData::Search {
            call, query, path, ..
        } => AgentActivityData::Search {
            call: normalize_call(call),
            query: query.clone(),
            path: path.clone(),
            result: None,
        },
        AgentActivityData::Command {
            call,
            command,
            args,
            cwd,
            ..
        } => AgentActivityData::Command {
            call: normalize_call(call),
            command: command.clone(),
            args: args.clone(),
            cwd: cwd.clone(),
            result: None,
        },
    }
}

fn restore_activity_tool_data(
    data: AgentActivityData,
    activity_id: ActivityId,
    calls: &BTreeMap<loom_core::ToolCallId, loom_model::ToolCall>,
    attempts: &BTreeMap<ActivityId, Option<ToolResult>>,
) -> Result<AgentActivityData> {
    let Some((stored_call, _)) = activity_tool_data(&data) else {
        return Ok(data);
    };
    let call = calls.get(&stored_call.id).cloned().ok_or_else(|| {
        LoomError::new(
            ErrorCode::MalformedPayload,
            "persisted tool activity has no logical tool-call row",
            false,
        )
    })?;
    let result = attempts.get(&activity_id).cloned().ok_or_else(|| {
        LoomError::new(
            ErrorCode::MalformedPayload,
            "persisted tool activity has no tool-attempt row",
            false,
        )
    })?;
    Ok(match data {
        AgentActivityData::ModelTurn { model } => AgentActivityData::ModelTurn { model },
        AgentActivityData::ToolCall { .. } => AgentActivityData::ToolCall { call, result },
        AgentActivityData::File {
            operation, path, ..
        } => AgentActivityData::File {
            call,
            operation,
            path,
            result,
        },
        AgentActivityData::Search { query, path, .. } => AgentActivityData::Search {
            call,
            query,
            path,
            result,
        },
        AgentActivityData::Command {
            command, args, cwd, ..
        } => AgentActivityData::Command {
            call,
            command,
            args,
            cwd,
            result,
        },
    })
}

fn save_run_tool_rows(
    transaction: &Transaction<'_>,
    activities_by_run: &DurableRunActivities,
    summaries: Option<&BTreeMap<RunId, DurableRunSummary>>,
) -> Result<()> {
    transaction
        .execute_batch(
            "CREATE TEMP TABLE IF NOT EXISTS _loom_wanted_run_tool_calls (
                run_id BLOB NOT NULL, tool_call_id BLOB NOT NULL,
                PRIMARY KEY(run_id, tool_call_id)
             ) WITHOUT ROWID, STRICT;
             CREATE TEMP TABLE IF NOT EXISTS _loom_wanted_run_tool_attempts (
                run_id BLOB NOT NULL, activity_id BLOB NOT NULL,
                PRIMARY KEY(run_id, activity_id)
             ) WITHOUT ROWID, STRICT;
             DELETE FROM _loom_wanted_run_tool_calls;
             DELETE FROM _loom_wanted_run_tool_attempts;",
        )
        .map_err(|error| {
            persistence_error(format!("could not stage run tool rows: {error}"), true)
        })?;
    for (run_id, activities) in activities_by_run {
        let run_id_bytes = run_id.as_uuid().as_bytes();
        let session_id: Vec<u8> = transaction
            .query_row(
                "SELECT session_id FROM run_summaries WHERE run_id=?1",
                [run_id_bytes.as_slice()],
                |row| row.get(0),
            )
            .map_err(|error| {
                persistence_error(
                    format!("run {run_id} has no durable summary for its tool rows: {error}"),
                    true,
                )
            })?;
        let execution = summaries
            .and_then(|summaries| summaries.get(run_id))
            .and_then(|summary| summary.execution_state.as_ref());
        let mut logical_calls =
            BTreeMap::<loom_core::ToolCallId, (&loom_model::ToolCall, Timestamp)>::new();
        let mut attempt_numbers = BTreeMap::<loom_core::ToolCallId, u32>::new();
        for activity in activities {
            if activity.run_id != *run_id {
                return Err(LoomError::invalid_request(
                    "run tool activity must match its owning run",
                ));
            }
            let Some((call, result)) = activity_tool_data(&activity.data) else {
                continue;
            };
            if let Some((existing, _)) = logical_calls.get(&call.id) {
                if existing.name != call.name || existing.arguments != call.arguments {
                    return Err(LoomError::new(
                        ErrorCode::MalformedPayload,
                        "a logical tool call changed its name or arguments",
                        false,
                    ));
                }
            } else {
                logical_calls.insert(call.id, (call, activity.started_at));
                let arguments = serde_json::to_vec(&call.arguments).map_err(|error| {
                    LoomError::new(
                        ErrorCode::MalformedPayload,
                        format!("could not encode tool-call arguments: {error}"),
                        false,
                    )
                })?;
                if arguments.len() > MAX_TOOL_ARGUMENT_BYTES {
                    return Err(LoomError::new(
                        ErrorCode::Persistence,
                        "tool-call arguments exceed the maximum supported size",
                        false,
                    ));
                }
                let arguments_hash = store_content(transaction, &arguments)?;
                let created_at = encode_timestamp(activity.started_at)?;
                let existing: Option<(Vec<u8>, String, Vec<u8>)> = transaction
                    .query_row(
                        "SELECT session_id, name, arguments_hash FROM run_tool_calls
                         WHERE run_id=?1 AND tool_call_id=?2",
                        params![
                            run_id_bytes.as_slice(),
                            call.id.as_uuid().as_bytes().as_slice()
                        ],
                        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                    )
                    .optional()
                    .map_err(|error| {
                        persistence_error(
                            format!("could not read logical tool call: {error}"),
                            true,
                        )
                    })?;
                if let Some((stored_session, stored_name, stored_arguments_hash)) = existing {
                    if stored_session != session_id
                        || stored_name != call.name
                        || stored_arguments_hash != arguments_hash
                    {
                        return Err(LoomError::new(
                            ErrorCode::MalformedPayload,
                            "persisted logical tool call is immutable",
                            false,
                        ));
                    }
                } else {
                    transaction
                        .execute(
                            "INSERT INTO run_tool_calls(
                                run_id, session_id, tool_call_id, name, arguments_hash, created_at
                             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                            params![
                                run_id_bytes.as_slice(),
                                session_id.as_slice(),
                                call.id.as_uuid().as_bytes().as_slice(),
                                call.name.as_str(),
                                arguments_hash,
                                created_at,
                            ],
                        )
                        .map_err(|error| {
                            persistence_error(
                                format!("could not save logical tool call {}: {error}", call.id),
                                true,
                            )
                        })?;
                }
                transaction
                    .execute(
                        "INSERT INTO _loom_wanted_run_tool_calls(run_id, tool_call_id)
                         VALUES (?1, ?2)",
                        params![
                            run_id_bytes.as_slice(),
                            call.id.as_uuid().as_bytes().as_slice()
                        ],
                    )
                    .map_err(|error| {
                        persistence_error(format!("could not stage tool call: {error}"), true)
                    })?;
            }

            let attempt_number = attempt_numbers.entry(call.id).or_default();
            *attempt_number = attempt_number.checked_add(1).ok_or_else(|| {
                LoomError::new(
                    ErrorCode::Persistence,
                    "tool call has too many execution attempts",
                    false,
                )
            })?;
            let state = match activity.status {
                AgentActivityStatus::Started
                    if execution
                        .and_then(|execution| execution.pending_tool_execution.as_ref())
                        .is_some_and(|pending| pending.id == call.id) =>
                {
                    AgentToolAttemptState::Queued
                }
                AgentActivityStatus::Started
                    if execution
                        .and_then(|execution| execution.last_failed_call.as_ref())
                        .is_some_and(|failed| failed.id == call.id) =>
                {
                    AgentToolAttemptState::OutcomeUnknown
                }
                AgentActivityStatus::Started => AgentToolAttemptState::Running,
                AgentActivityStatus::Completed => AgentToolAttemptState::Completed,
                AgentActivityStatus::Failed => AgentToolAttemptState::Failed,
                AgentActivityStatus::AwaitingApproval => AgentToolAttemptState::AwaitingApproval,
                AgentActivityStatus::AwaitingInput => AgentToolAttemptState::AwaitingInput,
                AgentActivityStatus::Cancelled => AgentToolAttemptState::Cancelled,
            };
            let result_hash = result
                .map(|result| {
                    let result = serde_json::to_vec(result).map_err(|error| {
                        LoomError::new(
                            ErrorCode::MalformedPayload,
                            format!("could not encode tool result: {error}"),
                            false,
                        )
                    })?;
                    store_content(transaction, &result)
                })
                .transpose()?;
            let started_at = encode_timestamp(activity.started_at)?;
            let completed_at = activity.completed_at.map(encode_timestamp).transpose()?;
            let activity_id = activity.id.as_uuid().as_bytes();
            transaction
                .execute(
                    "INSERT INTO run_tool_attempts(
                        run_id, session_id, activity_id, tool_call_id, attempt_number, state,
                        started_at, completed_at, result_hash
                     ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
                     ON CONFLICT(run_id, activity_id) DO UPDATE SET
                        session_id=excluded.session_id,
                        tool_call_id=excluded.tool_call_id,
                        attempt_number=excluded.attempt_number,
                        state=excluded.state,
                        started_at=excluded.started_at,
                        completed_at=excluded.completed_at,
                        result_hash=excluded.result_hash
                     WHERE run_tool_attempts.session_id IS NOT excluded.session_id
                        OR run_tool_attempts.tool_call_id IS NOT excluded.tool_call_id
                        OR run_tool_attempts.attempt_number IS NOT excluded.attempt_number
                        OR run_tool_attempts.state IS NOT excluded.state
                        OR run_tool_attempts.started_at IS NOT excluded.started_at
                        OR run_tool_attempts.completed_at IS NOT excluded.completed_at
                        OR run_tool_attempts.result_hash IS NOT excluded.result_hash",
                    params![
                        run_id_bytes.as_slice(),
                        session_id.as_slice(),
                        activity_id,
                        call.id.as_uuid().as_bytes().as_slice(),
                        i64::from(*attempt_number),
                        tool_attempt_state_name(state),
                        started_at,
                        completed_at,
                        result_hash,
                    ],
                )
                .map_err(|error| {
                    persistence_error(
                        format!("could not save tool attempt {}: {error}", activity.id),
                        true,
                    )
                })?;
            transaction
                .execute(
                    "INSERT INTO _loom_wanted_run_tool_attempts(run_id, activity_id)
                     VALUES (?1, ?2)",
                    params![run_id_bytes.as_slice(), activity_id],
                )
                .map_err(|error| {
                    persistence_error(format!("could not stage tool attempt: {error}"), true)
                })?;
        }
        transaction
            .execute(
                "DELETE FROM run_tool_attempts
                 WHERE run_id=?1 AND NOT EXISTS (
                    SELECT 1 FROM _loom_wanted_run_tool_attempts wanted
                    WHERE wanted.run_id=run_tool_attempts.run_id
                      AND wanted.activity_id=run_tool_attempts.activity_id
                 )",
                [run_id_bytes.as_slice()],
            )
            .map_err(|error| {
                persistence_error(
                    format!("could not prune tool attempts for {run_id}: {error}"),
                    true,
                )
            })?;
        transaction
            .execute(
                "DELETE FROM run_tool_calls
                 WHERE run_id=?1 AND NOT EXISTS (
                    SELECT 1 FROM _loom_wanted_run_tool_calls wanted
                    WHERE wanted.run_id=run_tool_calls.run_id
                      AND wanted.tool_call_id=run_tool_calls.tool_call_id
                 )",
                [run_id_bytes.as_slice()],
            )
            .map_err(|error| {
                persistence_error(
                    format!("could not prune tool calls for {run_id}: {error}"),
                    true,
                )
            })?;
    }
    collect_unused_content(transaction, MAX_CONTENT_GC_CANDIDATES_PER_WRITE)
}

fn save_filesystem_records(
    transaction: &Transaction<'_>,
    records: &[DurableFilesystemRecord],
) -> Result<()> {
    for record in records {
        let filesystem = record.payload.get("filesystem").ok_or_else(|| {
            LoomError::new(
                ErrorCode::MalformedPayload,
                "filesystem record has no workspace snapshot",
                false,
            )
        })?;
        let next_sequence = filesystem
            .get("next_sequence")
            .and_then(Value::as_u64)
            .ok_or_else(|| {
                LoomError::new(
                    ErrorCode::MalformedPayload,
                    "filesystem record has no change sequence high-water mark",
                    false,
                )
            })?;
        if filesystem.get("root").and_then(Value::as_str) != Some(record.root.as_str())
            || filesystem.get("control").and_then(Value::as_str)
                != Some(workspace_control_name(record.control))
            || filesystem.get("session_id").and_then(Value::as_str)
                != Some(record.session_id.to_string().as_str())
            || filesystem
                .get("checkpoints")
                .and_then(Value::as_array)
                .is_none_or(|checkpoints| !checkpoints.is_empty())
            || filesystem
                .get("edits")
                .and_then(Value::as_array)
                .is_none_or(|edits| !edits.is_empty())
            || filesystem
                .get("changes")
                .and_then(Value::as_array)
                .is_none_or(|changes| !changes.is_empty())
        {
            return Err(LoomError::new(
                ErrorCode::MalformedPayload,
                "filesystem record index fields or checkpoint payload are invalid",
                false,
            ));
        }
        let raw = serde_json::to_vec(&record.payload).map_err(|error| {
            persistence_error(
                format!("could not encode filesystem record: {error}"),
                false,
            )
        })?;
        if raw.len() > MAX_CONTENT_BYTES {
            return Err(LoomError::new(
                ErrorCode::Persistence,
                "filesystem record exceeds the maximum supported size",
                false,
            ));
        }
        let hash = Sha256::digest(&raw);
        let unchanged = transaction
            .query_row(
                "SELECT payload_hash FROM session_filesystems WHERE session_id=?1",
                [record.session_id.as_uuid().as_bytes().as_slice()],
                |row| row.get::<_, Vec<u8>>(0),
            )
            .optional()
            .map_err(|error| {
                persistence_error(
                    format!("could not compare filesystem record: {error}"),
                    true,
                )
            })?
            .is_some_and(|existing| existing == hash.as_slice());
        if !unchanged {
            let mut encoder = ZlibEncoder::new(Vec::new(), Compression::fast());
            encoder.write_all(&raw).map_err(|error| {
                persistence_error(
                    format!("could not compress filesystem record: {error}"),
                    false,
                )
            })?;
            let compressed = encoder.finish().map_err(|error| {
                persistence_error(
                    format!("could not compress filesystem record: {error}"),
                    false,
                )
            })?;
            let (codec, payload) = if compressed.len() < raw.len() {
                (1_i64, compressed)
            } else {
                (0_i64, raw.clone())
            };
            transaction
                .execute(
                    "INSERT INTO session_filesystems(
                        session_id, root, control, payload_hash, raw_size, payload_codec, payload
                     ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
                     ON CONFLICT(session_id) DO UPDATE SET
                        root=excluded.root,
                        control=excluded.control,
                        payload_hash=excluded.payload_hash,
                        raw_size=excluded.raw_size,
                        payload_codec=excluded.payload_codec,
                        payload=excluded.payload
                     WHERE session_filesystems.root IS NOT excluded.root
                        OR session_filesystems.control IS NOT excluded.control
                        OR session_filesystems.payload_hash IS NOT excluded.payload_hash",
                    params![
                        record.session_id.as_uuid().as_bytes().as_slice(),
                        record.root,
                        workspace_control_name(record.control),
                        hash.as_slice(),
                        raw.len() as i64,
                        codec,
                        payload,
                    ],
                )
                .map_err(|error| {
                    persistence_error(
                        format!(
                            "could not save filesystem record for {}: {error}",
                            record.session_id
                        ),
                        true,
                    )
                })?;
        }
        let next_sequence = encode_counter(next_sequence, "filesystem sequence")?;
        transaction
            .execute(
                "INSERT INTO filesystem_change_state(session_id, next_sequence)
                 VALUES (?1, ?2)
                 ON CONFLICT(session_id) DO UPDATE SET
                    next_sequence=MAX(filesystem_change_state.next_sequence, excluded.next_sequence)",
                params![record.session_id.as_uuid().as_bytes().as_slice(), next_sequence],
            )
            .map_err(|error| {
                persistence_error(format!("could not save filesystem sequence: {error}"), true)
            })?;
        if let Some(delta) = &record.delta {
            save_checkpoint_delta_rows(
                transaction,
                record.session_id,
                &record.checkpoints,
                &delta.deleted_checkpoints,
            )?;
            save_filesystem_edit_delta_rows(
                transaction,
                record.session_id,
                &record.edits,
                &delta.deleted_edits,
            )?;
            save_filesystem_change_rows(transaction, record.session_id, &record.changes)?;
            for sequence in &delta.deleted_changes {
                transaction
                    .execute(
                        "DELETE FROM filesystem_changes WHERE session_id=?1 AND sequence=?2",
                        params![
                            record.session_id.as_uuid().as_bytes().as_slice(),
                            encode_counter(sequence.value(), "filesystem sequence")?
                        ],
                    )
                    .map_err(|error| {
                        persistence_error(
                            format!("could not delete filesystem change: {error}"),
                            true,
                        )
                    })?;
            }
        } else {
            save_checkpoint_rows(transaction, record.session_id, &record.checkpoints)?;
            save_filesystem_edit_rows(transaction, record.session_id, &record.edits)?;
            save_filesystem_change_rows(transaction, record.session_id, &record.changes)?;
        }
        save_session_repository_rows(transaction, record.session_id, &record.repositories)?;
        save_session_directory_rows(transaction, record.session_id, &record.directories)?;
    }
    collect_unused_content(transaction, MAX_CONTENT_GC_CANDIDATES_PER_WRITE)?;
    Ok(())
}

fn save_filesystem_edit_rows(
    transaction: &Transaction<'_>,
    session_id: AgentSessionId,
    edits: &[DurableFilesystemEdit],
) -> Result<()> {
    transaction
        .execute(
            "DELETE FROM filesystem_edits WHERE session_id=?1",
            [session_id.as_uuid().as_bytes().as_slice()],
        )
        .map_err(|error| {
            persistence_error(format!("could not replace filesystem edits: {error}"), true)
        })?;
    for edit in edits {
        if edit.path.is_empty()
            || edit.path.len() > 4096
            || edit.after_revision.len() > 256
            || edit.before.as_ref().is_some_and(|text| {
                edit.before_bytes
                    .as_ref()
                    .is_some_and(|bytes| bytes != text.as_bytes())
            })
        {
            return Err(LoomError::new(
                ErrorCode::MalformedPayload,
                "filesystem edit metadata is invalid",
                false,
            ));
        }
        let before = edit
            .before_bytes
            .as_deref()
            .or_else(|| edit.before.as_ref().map(String::as_bytes));
        let before_hash = before
            .map(|bytes| store_content(transaction, bytes))
            .transpose()?;
        transaction
            .execute(
                "INSERT INTO filesystem_edits(
                    session_id, edit_id, path, before_hash, after_revision, source
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                 ON CONFLICT(session_id, edit_id) DO UPDATE SET
                    path=excluded.path,
                    before_hash=excluded.before_hash,
                    after_revision=excluded.after_revision,
                    source=excluded.source
                 WHERE filesystem_edits.path IS NOT excluded.path
                    OR filesystem_edits.before_hash IS NOT excluded.before_hash
                    OR filesystem_edits.after_revision IS NOT excluded.after_revision
                    OR filesystem_edits.source IS NOT excluded.source",
                params![
                    session_id.as_uuid().as_bytes().as_slice(),
                    i64::try_from(edit.id).map_err(|_| LoomError::new(
                        ErrorCode::Persistence,
                        "invalid filesystem edit id",
                        false
                    ))?,
                    edit.path,
                    before_hash,
                    edit.after_revision,
                    workspace_control_name(edit.source),
                ],
            )
            .map_err(|error| {
                persistence_error(format!("could not save filesystem edit: {error}"), true)
            })?;
    }
    Ok(())
}

fn save_filesystem_edit_delta_rows(
    transaction: &Transaction<'_>,
    session_id: AgentSessionId,
    edits: &[DurableFilesystemEdit],
    deleted: &[u64],
) -> Result<()> {
    for id in deleted {
        transaction
            .execute(
                "DELETE FROM filesystem_edits WHERE session_id=?1 AND edit_id=?2",
                params![
                    session_id.as_uuid().as_bytes().as_slice(),
                    i64::try_from(*id)
                        .map_err(|_| LoomError::invalid_request("invalid filesystem edit id"))?
                ],
            )
            .map_err(|error| {
                persistence_error(format!("could not delete filesystem edit: {error}"), true)
            })?;
    }
    for edit in edits {
        if edit.id == 0
            || edit.path.is_empty()
            || edit.path.len() > 4096
            || edit.after_revision.len() > 256
        {
            return Err(LoomError::new(
                ErrorCode::MalformedPayload,
                "filesystem edit metadata is invalid",
                false,
            ));
        }
        let before = edit
            .before_bytes
            .as_deref()
            .or_else(|| edit.before.as_ref().map(String::as_bytes));
        let before_hash = before
            .map(|bytes| store_content(transaction, bytes))
            .transpose()?;
        transaction.execute("INSERT INTO filesystem_edits(session_id,edit_id,path,before_hash,after_revision,source) VALUES(?1,?2,?3,?4,?5,?6) ON CONFLICT(session_id,edit_id) DO UPDATE SET path=excluded.path,before_hash=excluded.before_hash,after_revision=excluded.after_revision,source=excluded.source", params![session_id.as_uuid().as_bytes().as_slice(), i64::try_from(edit.id).map_err(|_| LoomError::invalid_request("invalid filesystem edit id"))?, edit.path, before_hash, edit.after_revision, workspace_control_name(edit.source)]).map_err(|error| persistence_error(format!("could not save filesystem edit delta: {error}"), true))?;
    }
    Ok(())
}

fn save_checkpoint_delta_rows(
    transaction: &Transaction<'_>,
    session_id: AgentSessionId,
    checkpoints: &[Checkpoint],
    deleted: &[CheckpointId],
) -> Result<()> {
    for checkpoint in checkpoints {
        if checkpoint.session_id != session_id || checkpoint.label.trim().is_empty() {
            return Err(LoomError::new(
                ErrorCode::MalformedPayload,
                "checkpoint identity or label is invalid",
                false,
            ));
        }
        let sid = session_id.as_uuid().as_bytes();
        let cid = checkpoint.id.as_uuid().as_bytes();
        transaction.execute("INSERT INTO checkpoints(session_id,checkpoint_id,label,created_at) VALUES(?1,?2,?3,?4) ON CONFLICT(session_id,checkpoint_id) DO UPDATE SET label=excluded.label,created_at=excluded.created_at WHERE checkpoints.label IS NOT excluded.label OR checkpoints.created_at IS NOT excluded.created_at", params![sid.as_slice(),cid.as_slice(),checkpoint.label,encode_timestamp(checkpoint.created_at)?]).map_err(|error| persistence_error(format!("could not save checkpoint delta: {error}"), true))?;
        let mut wanted = BTreeSet::new();
        for (path, file) in &checkpoint.files {
            if path.trim().is_empty() || file.content.len() > MAX_CONTENT_BYTES {
                return Err(LoomError::new(
                    ErrorCode::MalformedPayload,
                    "checkpoint file is invalid",
                    false,
                ));
            }
            wanted.insert(path.clone());
            let hash = store_content(transaction, file.content.as_bytes())?;
            transaction.execute("INSERT INTO checkpoint_files(session_id,checkpoint_id,path,existed,revision,expected_revision,content_hash) VALUES(?1,?2,?3,?4,?5,?6,?7) ON CONFLICT(session_id,checkpoint_id,path) DO UPDATE SET existed=excluded.existed,revision=excluded.revision,expected_revision=excluded.expected_revision,content_hash=excluded.content_hash WHERE checkpoint_files.existed IS NOT excluded.existed OR checkpoint_files.revision IS NOT excluded.revision OR checkpoint_files.expected_revision IS NOT excluded.expected_revision OR checkpoint_files.content_hash IS NOT excluded.content_hash",params![sid.as_slice(),cid.as_slice(),path,if file.existed{1_i64}else{0_i64},file.revision,file.expected_revision,hash]).map_err(|error|persistence_error(format!("could not save checkpoint file delta: {error}"),true))?;
        }
        let mut statement = transaction
            .prepare("SELECT path FROM checkpoint_files WHERE session_id=?1 AND checkpoint_id=?2")
            .map_err(|error| {
                persistence_error(format!("could not inspect checkpoint files: {error}"), true)
            })?;
        let rows = statement
            .query_map(params![sid.as_slice(), cid.as_slice()], |row| {
                row.get::<_, String>(0)
            })
            .map_err(|error| {
                persistence_error(format!("could not inspect checkpoint files: {error}"), true)
            })?;
        let existing = rows
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|error| {
                persistence_error(format!("could not inspect checkpoint files: {error}"), true)
            })?;
        drop(statement);
        for path in existing {
            if !wanted.contains(&path) {
                transaction.execute("DELETE FROM checkpoint_files WHERE session_id=?1 AND checkpoint_id=?2 AND path=?3",params![sid.as_slice(),cid.as_slice(),path]).map_err(|error|persistence_error(format!("could not delete checkpoint file: {error}"),true))?;
            }
        }
    }
    for id in deleted {
        transaction
            .execute(
                "DELETE FROM checkpoints WHERE session_id=?1 AND checkpoint_id=?2",
                params![
                    session_id.as_uuid().as_bytes().as_slice(),
                    id.as_uuid().as_bytes().as_slice()
                ],
            )
            .map_err(|error| {
                persistence_error(format!("could not delete checkpoint: {error}"), true)
            })?;
    }
    Ok(())
}

fn save_filesystem_change_rows(
    transaction: &Transaction<'_>,
    session_id: AgentSessionId,
    changes: &[SessionFilesystemChange],
) -> Result<()> {
    let mut max_sequence = transaction
        .query_row(
            "SELECT MAX(sequence) FROM filesystem_changes WHERE session_id=?1",
            [session_id.as_uuid().as_bytes().as_slice()],
            |row| row.get::<_, Option<i64>>(0),
        )
        .map_err(|error| {
            persistence_error(
                format!("could not inspect filesystem change high-water mark: {error}"),
                true,
            )
        })?;
    let mut previous = None;
    for change in changes {
        if change.session_id != session_id
            || change.path.is_empty()
            || change.path.len() > 4096
            || change
                .revision
                .as_ref()
                .is_some_and(|revision| revision.len() > 256)
            || previous.is_some_and(|sequence| sequence >= change.sequence)
        {
            return Err(LoomError::new(
                ErrorCode::MalformedPayload,
                "filesystem change identity or ordering is invalid",
                false,
            ));
        }
        previous = Some(change.sequence);
        let sequence = encode_counter(change.sequence.value(), "filesystem sequence")?;
        if max_sequence.is_some_and(|current| sequence <= current) {
            continue;
        }
        let session_id_bytes = session_id.as_uuid().as_bytes();
        transaction
            .execute(
                "INSERT INTO filesystem_changes(session_id, sequence, path, kind, revision)
                 VALUES (?1, ?2, ?3, ?4, ?5)
                 ON CONFLICT(session_id, sequence) DO UPDATE SET
                    path=excluded.path,
                    kind=excluded.kind,
                    revision=excluded.revision
                 WHERE filesystem_changes.path IS NOT excluded.path
                    OR filesystem_changes.kind IS NOT excluded.kind
                    OR filesystem_changes.revision IS NOT excluded.revision",
                params![
                    session_id_bytes.as_slice(),
                    sequence,
                    change.path,
                    workspace_change_kind_name(change.kind),
                    change.revision,
                ],
            )
            .map_err(|error| {
                persistence_error(format!("could not save filesystem change: {error}"), true)
            })?;
        max_sequence = Some(sequence);
    }
    transaction
        .execute(
            "DELETE FROM filesystem_changes
             WHERE session_id=?1 AND sequence < COALESCE((
                SELECT sequence FROM filesystem_changes
                WHERE session_id=?1 ORDER BY sequence DESC
                LIMIT 1 OFFSET ?2
             ), -1)",
            params![
                session_id.as_uuid().as_bytes().as_slice(),
                (MAX_FILESYSTEM_CHANGE_HISTORY - 1) as i64
            ],
        )
        .map_err(|error| {
            persistence_error(format!("could not prune filesystem changes: {error}"), true)
        })?;
    Ok(())
}

fn save_session_repository_rows(
    transaction: &Transaction<'_>,
    session_id: AgentSessionId,
    repositories: &BTreeMap<RepositoryId, SessionRepository>,
) -> Result<()> {
    transaction
        .execute_batch(
            "CREATE TEMP TABLE IF NOT EXISTS _loom_wanted_session_repositories (
            session_id BLOB NOT NULL, repository_id BLOB NOT NULL,
            PRIMARY KEY(session_id, repository_id)
        ) WITHOUT ROWID;",
        )
        .map_err(|error| {
            persistence_error(format!("could not stage repositories: {error}"), true)
        })?;
    let session_bytes = session_id.as_uuid().as_bytes();
    transaction
        .execute(
            "DELETE FROM _loom_wanted_session_repositories WHERE session_id=?1",
            [session_bytes.as_slice()],
        )
        .map_err(|error| {
            persistence_error(format!("could not reset repository staging: {error}"), true)
        })?;
    for (id, repository) in repositories {
        if *id != repository.id || repository.path.is_empty() || repository.path.len() > 4096 {
            return Err(LoomError::new(
                ErrorCode::MalformedPayload,
                "session repository metadata is invalid",
                false,
            ));
        }
        let repository_bytes = id.as_uuid().as_bytes();
        let attached_at = encode_timestamp(repository.attached_at)?;
        transaction.execute(
            "INSERT INTO _loom_wanted_session_repositories(session_id, repository_id) VALUES (?1, ?2)",
            params![session_bytes.as_slice(), repository_bytes.as_slice()],
        ).map_err(|error| persistence_error(format!("could not stage repository: {error}"), true))?;
        transaction.execute(
            "INSERT INTO session_repositories(session_id, repository_id, source, path, revision, attached_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(session_id, repository_id) DO UPDATE SET
                source=excluded.source, path=excluded.path, revision=excluded.revision, attached_at=excluded.attached_at
             WHERE session_repositories.source IS NOT excluded.source
                OR session_repositories.path IS NOT excluded.path
                OR session_repositories.revision IS NOT excluded.revision
                OR session_repositories.attached_at IS NOT excluded.attached_at",
            params![session_bytes.as_slice(), repository_bytes.as_slice(), repository.source, repository.path,
                repository.revision, attached_at],
        ).map_err(|error| persistence_error(format!("could not save session repository: {error}"), true))?;
    }
    transaction
        .execute(
            "DELETE FROM session_repositories WHERE session_id=?1 AND NOT EXISTS (
            SELECT 1 FROM _loom_wanted_session_repositories wanted
            WHERE wanted.session_id=session_repositories.session_id
              AND wanted.repository_id=session_repositories.repository_id
        )",
            [session_bytes.as_slice()],
        )
        .map_err(|error| {
            persistence_error(
                format!("could not prune session repositories: {error}"),
                true,
            )
        })?;
    Ok(())
}

fn save_session_directory_rows(
    transaction: &Transaction<'_>,
    session_id: AgentSessionId,
    directories: &[SessionDirectory],
) -> Result<()> {
    transaction
        .execute_batch(
            "CREATE TEMP TABLE IF NOT EXISTS _loom_wanted_session_directories (
            session_id BLOB NOT NULL, ordinal INTEGER NOT NULL,
            PRIMARY KEY(session_id, ordinal)
        ) WITHOUT ROWID;",
        )
        .map_err(|error| {
            persistence_error(
                format!("could not stage mounted directories: {error}"),
                true,
            )
        })?;
    let session_bytes = session_id.as_uuid().as_bytes();
    transaction
        .execute(
            "DELETE FROM _loom_wanted_session_directories WHERE session_id=?1",
            [session_bytes.as_slice()],
        )
        .map_err(|error| {
            persistence_error(format!("could not reset directory staging: {error}"), true)
        })?;
    let mut unique_paths = BTreeSet::new();
    for (ordinal, directory) in directories.iter().enumerate() {
        if directory.path.is_empty()
            || directory.path.len() > 4096
            || !unique_paths.insert(&directory.path)
        {
            return Err(LoomError::new(
                ErrorCode::MalformedPayload,
                "mounted directory metadata is invalid",
                false,
            ));
        }
        let ordinal = i64::try_from(ordinal).map_err(|_| {
            LoomError::new(
                ErrorCode::Persistence,
                "mounted directory count exceeds SQLite's integer range",
                false,
            )
        })?;
        transaction
            .execute(
                "INSERT INTO _loom_wanted_session_directories(session_id, ordinal) VALUES (?1, ?2)",
                params![session_bytes.as_slice(), ordinal],
            )
            .map_err(|error| {
                persistence_error(format!("could not stage mounted directory: {error}"), true)
            })?;
        transaction.execute(
            "INSERT INTO session_directories(session_id, ordinal, source, path) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(session_id, ordinal) DO UPDATE SET source=excluded.source, path=excluded.path
             WHERE session_directories.source IS NOT excluded.source OR session_directories.path IS NOT excluded.path",
            params![session_bytes.as_slice(), ordinal, directory.source, directory.path],
        ).map_err(|error| persistence_error(format!("could not save mounted directory: {error}"), true))?;
    }
    transaction.execute(
        "DELETE FROM session_directories WHERE session_id=?1 AND NOT EXISTS (
            SELECT 1 FROM _loom_wanted_session_directories wanted
            WHERE wanted.session_id=session_directories.session_id AND wanted.ordinal=session_directories.ordinal
        )", [session_bytes.as_slice()],
    ).map_err(|error| persistence_error(format!("could not prune mounted directories: {error}"), true))?;
    Ok(())
}

fn save_checkpoint_rows(
    transaction: &Transaction<'_>,
    session_id: AgentSessionId,
    checkpoints: &[Checkpoint],
) -> Result<()> {
    transaction
        .execute_batch(
            "CREATE TEMP TABLE IF NOT EXISTS _loom_wanted_checkpoints (
                session_id BLOB NOT NULL,
                checkpoint_id BLOB NOT NULL,
                PRIMARY KEY(session_id, checkpoint_id)
             ) WITHOUT ROWID, STRICT;
             CREATE TEMP TABLE IF NOT EXISTS _loom_wanted_checkpoint_files (
                session_id BLOB NOT NULL,
                checkpoint_id BLOB NOT NULL,
                path TEXT NOT NULL,
                PRIMARY KEY(session_id, checkpoint_id, path)
             ) WITHOUT ROWID, STRICT;
             DELETE FROM _loom_wanted_checkpoints;
             DELETE FROM _loom_wanted_checkpoint_files;",
        )
        .map_err(|error| {
            persistence_error(
                format!("could not stage filesystem checkpoints: {error}"),
                true,
            )
        })?;
    let session_id_bytes = session_id.as_uuid().as_bytes();
    for checkpoint in checkpoints {
        if checkpoint.session_id != session_id || checkpoint.label.trim().is_empty() {
            return Err(LoomError::new(
                ErrorCode::MalformedPayload,
                "checkpoint identity or label is invalid",
                false,
            ));
        }
        let checkpoint_id_bytes = checkpoint.id.as_uuid().as_bytes();
        transaction
            .execute(
                "INSERT INTO _loom_wanted_checkpoints(session_id, checkpoint_id)
                 VALUES (?1, ?2)",
                params![session_id_bytes.as_slice(), checkpoint_id_bytes.as_slice()],
            )
            .map_err(|error| {
                persistence_error(
                    format!("could not stage checkpoint {}: {error}", checkpoint.id),
                    true,
                )
            })?;
        transaction
            .execute(
                "INSERT INTO checkpoints(session_id, checkpoint_id, label, created_at)
                 VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(session_id, checkpoint_id) DO UPDATE SET
                    label=excluded.label, created_at=excluded.created_at
                 WHERE checkpoints.label IS NOT excluded.label
                    OR checkpoints.created_at IS NOT excluded.created_at",
                params![
                    session_id_bytes.as_slice(),
                    checkpoint_id_bytes.as_slice(),
                    checkpoint.label,
                    encode_timestamp(checkpoint.created_at)?,
                ],
            )
            .map_err(|error| {
                persistence_error(
                    format!("could not save checkpoint {}: {error}", checkpoint.id),
                    true,
                )
            })?;
        for (path, file) in &checkpoint.files {
            if path.trim().is_empty() || file.content.len() > MAX_CONTENT_BYTES {
                return Err(LoomError::new(
                    ErrorCode::Persistence,
                    "checkpoint file path or content is invalid",
                    false,
                ));
            }
            let content_hash = store_content(transaction, file.content.as_bytes())?;
            transaction
                .execute(
                    "INSERT INTO _loom_wanted_checkpoint_files(session_id, checkpoint_id, path)
                     VALUES (?1, ?2, ?3)",
                    params![
                        session_id_bytes.as_slice(),
                        checkpoint_id_bytes.as_slice(),
                        path
                    ],
                )
                .map_err(|error| {
                    persistence_error(
                        format!("could not stage checkpoint file '{path}': {error}"),
                        true,
                    )
                })?;
            transaction
                .execute(
                    "INSERT INTO checkpoint_files(
                        session_id, checkpoint_id, path, existed, revision,
                        expected_revision, content_hash
                     ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
                     ON CONFLICT(session_id, checkpoint_id, path) DO UPDATE SET
                        existed=excluded.existed,
                        revision=excluded.revision,
                        expected_revision=excluded.expected_revision,
                        content_hash=excluded.content_hash
                     WHERE checkpoint_files.existed IS NOT excluded.existed
                        OR checkpoint_files.revision IS NOT excluded.revision
                        OR checkpoint_files.expected_revision IS NOT excluded.expected_revision
                        OR checkpoint_files.content_hash IS NOT excluded.content_hash",
                    params![
                        session_id_bytes.as_slice(),
                        checkpoint_id_bytes.as_slice(),
                        path,
                        if file.existed { 1_i64 } else { 0_i64 },
                        file.revision,
                        file.expected_revision,
                        content_hash,
                    ],
                )
                .map_err(|error| {
                    persistence_error(
                        format!("could not save checkpoint file '{path}': {error}"),
                        true,
                    )
                })?;
        }
        transaction
            .execute(
                "DELETE FROM checkpoint_files
                 WHERE session_id=?1 AND checkpoint_id=?2 AND NOT EXISTS (
                    SELECT 1 FROM _loom_wanted_checkpoint_files wanted
                    WHERE wanted.session_id=checkpoint_files.session_id
                      AND wanted.checkpoint_id=checkpoint_files.checkpoint_id
                      AND wanted.path=checkpoint_files.path
                 )",
                params![session_id_bytes.as_slice(), checkpoint_id_bytes.as_slice()],
            )
            .map_err(|error| {
                persistence_error(format!("could not prune checkpoint files: {error}"), true)
            })?;
    }
    transaction
        .execute(
            "DELETE FROM checkpoints
             WHERE session_id=?1 AND NOT EXISTS (
                SELECT 1 FROM _loom_wanted_checkpoints wanted
                WHERE wanted.session_id=checkpoints.session_id
                  AND wanted.checkpoint_id=checkpoints.checkpoint_id
             )",
            [session_id_bytes.as_slice()],
        )
        .map_err(|error| {
            persistence_error(format!("could not prune checkpoints: {error}"), true)
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

fn save_feed_rows(transaction: &Transaction<'_>, feed: &DurableFeedState) -> Result<()> {
    save_feed_rows_with_limits(
        transaction,
        feed,
        MAX_DURABLE_FEED_SESSION_BYTES,
        MAX_DURABLE_FEED_TOTAL_BYTES,
    )
}

fn save_feed_rows_with_limits(
    transaction: &Transaction<'_>,
    feed: &DurableFeedState,
    session_byte_limit: usize,
    total_byte_limit: usize,
) -> Result<()> {
    save_feed_rows_with_limits_and_pruning(
        transaction,
        feed,
        session_byte_limit,
        total_byte_limit,
        true,
    )
}

fn save_feed_rows_incremental(
    transaction: &Transaction<'_>,
    feed: &DurableFeedState,
) -> Result<()> {
    save_feed_rows_with_limits_and_pruning(
        transaction,
        feed,
        MAX_DURABLE_FEED_SESSION_BYTES,
        MAX_DURABLE_FEED_TOTAL_BYTES,
        false,
    )
}

fn save_feed_rows_with_limits_and_pruning(
    transaction: &Transaction<'_>,
    feed: &DurableFeedState,
    session_byte_limit: usize,
    total_byte_limit: usize,
    prune: bool,
) -> Result<()> {
    let next_sequence = i64::try_from(feed.next_sequence.value()).map_err(|_| {
        LoomError::new(
            ErrorCode::Persistence,
            "event sequence exceeds SQLite's integer range",
            false,
        )
    })?;
    let retention_limit = i64::try_from(feed.retention_limit).map_err(|_| {
        LoomError::new(
            ErrorCode::Persistence,
            "event retention limit exceeds SQLite's integer range",
            false,
        )
    })?;
    let mut previous = 0;
    for event in &feed.events {
        let sequence = i64::try_from(event.sequence.value()).map_err(|_| {
            LoomError::new(
                ErrorCode::Persistence,
                "event sequence exceeds SQLite's integer range",
                false,
            )
        })?;
        if sequence <= previous || sequence > next_sequence {
            return Err(LoomError::new(
                ErrorCode::MalformedPayload,
                "event feed sequences are invalid",
                false,
            ));
        }
        previous = sequence;
        let workspace_id = transaction
            .query_row(
                "SELECT workspace_id FROM sessions WHERE id=?1",
                [event.session_id.as_uuid().as_bytes().as_slice()],
                |row| row.get::<_, Vec<u8>>(0),
            )
            .optional()
            .map_err(|error| {
                persistence_error(format!("could not resolve event workspace: {error}"), true)
            })?
            .ok_or_else(|| {
                LoomError::new(
                    ErrorCode::Persistence,
                    format!(
                        "cannot persist event for unknown session {}",
                        event.session_id
                    ),
                    false,
                )
            })?;
        let raw_payload = serde_json::to_vec(event).map_err(|error| {
            persistence_error(format!("could not encode event feed entry: {error}"), false)
        })?;
        if raw_payload.len() > MAX_FEED_EVENT_BYTES {
            return Err(LoomError::new(
                ErrorCode::Persistence,
                "event feed entry exceeds the maximum supported size",
                false,
            ));
        }
        let mut encoder = ZlibEncoder::new(Vec::new(), Compression::fast());
        encoder.write_all(&raw_payload).map_err(|error| {
            persistence_error(
                format!("could not compress event feed entry: {error}"),
                false,
            )
        })?;
        let compressed = encoder.finish().map_err(|error| {
            persistence_error(
                format!("could not compress event feed entry: {error}"),
                false,
            )
        })?;
        let (payload_codec, payload) = if compressed.len() < raw_payload.len() {
            (1_i64, compressed)
        } else {
            (0_i64, raw_payload)
        };
        transaction
            .execute(
                "INSERT INTO feed_events(sequence, session_id, workspace_id, payload_codec, payload)
                 VALUES (?1, ?2, ?3, ?4, ?5)
                 ON CONFLICT(sequence) DO NOTHING",
                params![
                    sequence,
                    event.session_id.as_uuid().as_bytes().as_slice(),
                    workspace_id.as_slice(),
                    payload_codec,
                    payload
                ],
            )
            .map_err(|error| {
                persistence_error(format!("could not save event feed entry: {error}"), true)
            })?;
        transaction
            .execute(
                "INSERT INTO feed_workspace_meta(
                    workspace_id, first_sequence, latest_sequence, pruned_through
                 ) VALUES (?1, ?2, ?2, 0)
                 ON CONFLICT(workspace_id) DO UPDATE SET
                    latest_sequence=MAX(feed_workspace_meta.latest_sequence, excluded.latest_sequence)",
                params![workspace_id.as_slice(), sequence],
            )
            .map_err(|error| persistence_error(format!("could not save workspace feed cursor: {error}"), true))?;
        transaction
            .execute(
                "INSERT INTO feed_session_meta(
                    session_id, first_sequence, latest_sequence, pruned_through
                 ) VALUES (?1, ?2, ?2, 0)
                 ON CONFLICT(session_id) DO UPDATE SET
                    latest_sequence=MAX(feed_session_meta.latest_sequence, excluded.latest_sequence)",
                params![
                    event.session_id.as_uuid().as_bytes().as_slice(),
                    sequence
                ],
            )
            .map_err(|error| {
                persistence_error(format!("could not save session feed cursor: {error}"), true)
            })?;
    }
    let mut previous_workspace = 0;
    for event in &feed.workspace_events {
        let sequence = i64::try_from(event.sequence.value()).map_err(|_| {
            LoomError::new(
                ErrorCode::Persistence,
                "workspace event sequence exceeds SQLite's integer range",
                false,
            )
        })?;
        if sequence <= previous_workspace || sequence > next_sequence {
            return Err(LoomError::new(
                ErrorCode::MalformedPayload,
                "workspace event feed sequences are invalid",
                false,
            ));
        }
        previous_workspace = sequence;
        let duplicate: bool = transaction
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM feed_events WHERE sequence=?1)",
                [sequence],
                |row| row.get(0),
            )
            .map_err(|error| {
                persistence_error(
                    format!("could not validate workspace event sequence: {error}"),
                    true,
                )
            })?;
        if duplicate {
            return Err(LoomError::new(
                ErrorCode::MalformedPayload,
                "workspace and session event sequences overlap",
                false,
            ));
        }
        let raw_payload = serde_json::to_vec(event).map_err(|error| {
            persistence_error(
                format!("could not encode workspace feed entry: {error}"),
                false,
            )
        })?;
        if raw_payload.len() > MAX_FEED_EVENT_BYTES {
            return Err(LoomError::new(
                ErrorCode::Persistence,
                "workspace event exceeds the maximum supported size",
                false,
            ));
        }
        let mut encoder = ZlibEncoder::new(Vec::new(), Compression::fast());
        encoder.write_all(&raw_payload).map_err(|error| {
            persistence_error(
                format!("could not compress workspace event: {error}"),
                false,
            )
        })?;
        let compressed = encoder.finish().map_err(|error| {
            persistence_error(
                format!("could not compress workspace event: {error}"),
                false,
            )
        })?;
        let (codec, payload) = if compressed.len() < raw_payload.len() {
            (1_i64, compressed)
        } else {
            (0_i64, raw_payload)
        };
        transaction
            .execute(
                "INSERT INTO workspace_feed_events(sequence, workspace_id, payload_codec, payload)
             VALUES (?1, ?2, ?3, ?4) ON CONFLICT(sequence) DO NOTHING",
                params![
                    sequence,
                    event.workspace_id.as_uuid().as_bytes().as_slice(),
                    codec,
                    payload
                ],
            )
            .map_err(|error| {
                persistence_error(format!("could not save workspace event: {error}"), true)
            })?;
        transaction.execute(
            "INSERT INTO feed_workspace_meta(workspace_id, first_sequence, latest_sequence, pruned_through)
             VALUES (?1, ?2, ?2, 0)
             ON CONFLICT(workspace_id) DO UPDATE SET latest_sequence=MAX(feed_workspace_meta.latest_sequence, excluded.latest_sequence)",
            params![event.workspace_id.as_uuid().as_bytes().as_slice(), sequence],
        ).map_err(|error| persistence_error(format!("could not save workspace feed cursor: {error}"), true))?;
    }
    save_feed_store_meta(transaction, next_sequence, retention_limit)?;
    if !prune {
        return Ok(());
    }
    transaction
        .execute_batch(
            "CREATE TEMP TABLE IF NOT EXISTS _loom_pruned_feed (
                sequence INTEGER PRIMARY KEY,
                session_id BLOB NOT NULL,
                workspace_id BLOB NOT NULL
             ) WITHOUT ROWID;
             DELETE FROM _loom_pruned_feed;",
        )
        .map_err(|error| {
            persistence_error(
                format!("could not prepare reconnect feed pruning: {error}"),
                true,
            )
        })?;
    transaction
        .execute(
            "WITH ranked AS (
                SELECT sequence, session_id, workspace_id,
                       SUM(length(payload)) OVER (
                           PARTITION BY session_id ORDER BY sequence DESC
                       ) AS session_bytes,
                       ROW_NUMBER() OVER (
                           PARTITION BY session_id ORDER BY sequence DESC
                       ) AS session_position,
                       SUM(length(payload)) OVER (
                           ORDER BY sequence DESC
                       ) + (SELECT COALESCE(SUM(length(payload)), 0)
                            FROM workspace_feed_events) AS total_bytes
                FROM feed_events
             )
             INSERT INTO _loom_pruned_feed(sequence, session_id, workspace_id)
             SELECT sequence, session_id, workspace_id FROM ranked
             WHERE session_bytes > ?1 OR session_position > ?2 OR total_bytes > ?3",
            params![
                session_byte_limit as i64,
                retention_limit,
                total_byte_limit as i64
            ],
        )
        .map_err(|error| {
            persistence_error(
                format!("could not select reconnect feed retention: {error}"),
                true,
            )
        })?;
    transaction
        .execute(
            "WITH pruned AS (
                SELECT workspace_id, MAX(sequence) AS pruned_through
                FROM _loom_pruned_feed GROUP BY workspace_id
             )
             UPDATE feed_workspace_meta
             SET pruned_through=MAX(
                    pruned_through,
                    (SELECT pruned.pruned_through FROM pruned
                     WHERE pruned.workspace_id=feed_workspace_meta.workspace_id)
                 )
             WHERE workspace_id IN (SELECT workspace_id FROM pruned)",
            [],
        )
        .map_err(|error| {
            persistence_error(
                format!("could not update pruned workspace feed cursors: {error}"),
                true,
            )
        })?;
    transaction
        .execute(
            "WITH pruned AS (
                SELECT session_id, MAX(sequence) AS pruned_through
                FROM _loom_pruned_feed GROUP BY session_id
             )
             UPDATE feed_session_meta
             SET pruned_through=MAX(
                    pruned_through,
                    (SELECT pruned.pruned_through FROM pruned
                     WHERE pruned.session_id=feed_session_meta.session_id)
                 )
             WHERE session_id IN (SELECT session_id FROM pruned)",
            [],
        )
        .map_err(|error| {
            persistence_error(
                format!("could not update pruned feed cursors: {error}"),
                true,
            )
        })?;
    transaction
        .execute(
            "DELETE FROM feed_events WHERE sequence IN (
                SELECT sequence FROM _loom_pruned_feed
             )",
            [],
        )
        .map_err(|error| persistence_error(format!("could not prune event feed: {error}"), true))?;
    transaction
        .execute_batch(
            "CREATE TEMP TABLE IF NOT EXISTS _loom_pruned_workspace_feed (
            sequence INTEGER PRIMARY KEY,
            workspace_id BLOB NOT NULL
         ) WITHOUT ROWID, STRICT;
         DELETE FROM _loom_pruned_workspace_feed;",
        )
        .map_err(|error| {
            persistence_error(
                format!("could not prepare workspace feed pruning: {error}"),
                true,
            )
        })?;
    transaction.execute(
        "WITH ranked AS (
            SELECT sequence, workspace_id,
                   SUM(length(payload)) OVER (PARTITION BY workspace_id ORDER BY sequence DESC) AS stream_bytes,
                   ROW_NUMBER() OVER (PARTITION BY workspace_id ORDER BY sequence DESC) AS stream_position,
                   SUM(length(payload)) OVER (ORDER BY sequence DESC)
                       + (SELECT COALESCE(SUM(length(payload)), 0) FROM feed_events) AS total_bytes
            FROM workspace_feed_events
         )
         INSERT INTO _loom_pruned_workspace_feed(sequence, workspace_id)
         SELECT sequence, workspace_id FROM ranked
         WHERE stream_bytes > ?1 OR stream_position > ?2 OR total_bytes > ?3",
        params![session_byte_limit as i64, retention_limit, total_byte_limit as i64],
    ).map_err(|error| persistence_error(format!("could not select workspace feed retention: {error}"), true))?;
    transaction
        .execute(
            "WITH pruned AS (
            SELECT workspace_id, MAX(sequence) AS pruned_through
            FROM _loom_pruned_workspace_feed GROUP BY workspace_id
         )
         UPDATE feed_workspace_meta
         SET pruned_through=MAX(pruned_through,
             (SELECT pruned.pruned_through FROM pruned
              WHERE pruned.workspace_id=feed_workspace_meta.workspace_id))
         WHERE workspace_id IN (SELECT workspace_id FROM pruned)",
            [],
        )
        .map_err(|error| {
            persistence_error(
                format!("could not update pruned workspace-only cursors: {error}"),
                true,
            )
        })?;
    transaction
        .execute(
            "DELETE FROM workspace_feed_events WHERE sequence IN (
            SELECT sequence FROM _loom_pruned_workspace_feed
         )",
            [],
        )
        .map_err(|error| {
            persistence_error(format!("could not prune workspace feed: {error}"), true)
        })?;
    Ok(())
}

fn save_feed_store_meta(
    transaction: &Transaction<'_>,
    next_sequence: i64,
    retention_limit: i64,
) -> Result<()> {
    transaction
        .execute(
            "INSERT INTO feed_store_meta(singleton, next_sequence, retention_limit)
             VALUES (1, ?1, ?2)
             ON CONFLICT(singleton) DO UPDATE SET
                next_sequence=MAX(feed_store_meta.next_sequence, excluded.next_sequence),
                retention_limit=excluded.retention_limit
             WHERE feed_store_meta.next_sequence < excluded.next_sequence
                OR feed_store_meta.retention_limit IS NOT excluded.retention_limit",
            params![next_sequence, retention_limit],
        )
        .map_err(|error| {
            persistence_error(format!("could not save event feed cursor: {error}"), true)
        })?;
    Ok(())
}

fn decode_feed_event(
    sequence: i64,
    session_id: Vec<u8>,
    payload_codec: i64,
    payload: Vec<u8>,
) -> Result<ServerEventEnvelope> {
    let payload = match payload_codec {
        0 => payload,
        1 => {
            let mut decoded = Vec::new();
            ZlibDecoder::new(payload.as_slice())
                .take(MAX_FEED_EVENT_BYTES as u64 + 1)
                .read_to_end(&mut decoded)
                .map_err(|error| {
                    LoomError::new(
                        ErrorCode::MalformedPayload,
                        format!("persisted event feed entry is malformed: {error}"),
                        false,
                    )
                })?;
            decoded
        }
        _ => {
            return Err(LoomError::new(
                ErrorCode::MalformedPayload,
                "persisted event feed entry uses an unsupported codec",
                false,
            ));
        }
    };
    if sequence <= 0 || payload.len() > MAX_FEED_EVENT_BYTES {
        return Err(LoomError::new(
            ErrorCode::MalformedPayload,
            "persisted event feed entry exceeds its supported size or sequence range",
            false,
        ));
    }
    let event: ServerEventEnvelope = serde_json::from_slice(&payload).map_err(|error| {
        LoomError::new(
            ErrorCode::MalformedPayload,
            format!("persisted event feed entry is malformed: {error}"),
            false,
        )
    })?;
    if event.sequence.value() != sequence as u64
        || event.session_id.as_uuid().as_bytes().as_slice() != session_id.as_slice()
    {
        return Err(LoomError::new(
            ErrorCode::MalformedPayload,
            "persisted event feed index does not match its payload",
            false,
        ));
    }
    Ok(event)
}

fn decode_feed_payload(payload_codec: i64, payload: Vec<u8>) -> Result<Vec<u8>> {
    let decoded = match payload_codec {
        0 => payload,
        1 => {
            let mut decoded = Vec::new();
            ZlibDecoder::new(payload.as_slice())
                .take(MAX_FEED_EVENT_BYTES as u64 + 1)
                .read_to_end(&mut decoded)
                .map_err(|error| {
                    LoomError::new(
                        ErrorCode::MalformedPayload,
                        format!("persisted event feed entry is malformed: {error}"),
                        false,
                    )
                })?;
            decoded
        }
        _ => {
            return Err(LoomError::new(
                ErrorCode::MalformedPayload,
                "persisted event feed entry uses an unsupported codec",
                false,
            ));
        }
    };
    if decoded.len() > MAX_FEED_EVENT_BYTES {
        return Err(LoomError::new(
            ErrorCode::MalformedPayload,
            "persisted event feed entry exceeds the maximum supported size",
            false,
        ));
    }
    Ok(decoded)
}

#[cfg(test)]
fn workspace_feed_event_sequence(event: &WorkspaceFeedEvent) -> u64 {
    match event {
        WorkspaceFeedEvent::Session(event) => event.sequence.value(),
        WorkspaceFeedEvent::Workspace(event) => event.sequence.value(),
    }
}

fn load_all_workspace_events(connection: &Connection) -> Result<Vec<WorkspaceEventEnvelope>> {
    let mut statement = connection
        .prepare(
            "SELECT sequence, workspace_id, payload_codec, payload
         FROM workspace_feed_events ORDER BY sequence",
        )
        .map_err(|error| {
            persistence_error(
                format!("could not prepare workspace event feed: {error}"),
                true,
            )
        })?;
    let rows = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, Vec<u8>>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, Vec<u8>>(3)?,
            ))
        })
        .map_err(|error| {
            persistence_error(
                format!("could not read workspace event feed: {error}"),
                true,
            )
        })?;
    rows.map(|row| {
        let (sequence, workspace_id, codec, payload) = row.map_err(|error| {
            persistence_error(format!("could not read workspace event: {error}"), true)
        })?;
        let payload = decode_feed_payload(codec, payload)?;
        let event: WorkspaceEventEnvelope = serde_json::from_slice(&payload).map_err(|error| {
            LoomError::new(
                ErrorCode::MalformedPayload,
                format!("persisted workspace event is malformed: {error}"),
                false,
            )
        })?;
        if sequence <= 0
            || event.sequence.value() != sequence as u64
            || event.workspace_id.as_uuid().as_bytes().as_slice() != workspace_id.as_slice()
        {
            return Err(LoomError::new(
                ErrorCode::MalformedPayload,
                "persisted workspace event index does not match its payload",
                false,
            ));
        }
        Ok(event)
    })
    .collect()
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

    #[derive(Debug, Deserialize, PartialEq, Serialize)]
    struct Fixture {
        value: String,
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
                    role: loom_model::MessageRole::System,
                    content: "old system".to_owned(),
                    name: None,
                    tool_call_id: None,
                    tool_calls: Vec::new(),
                },
                DurableRunMessage {
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
                    role: loom_model::MessageRole::User,
                    content: "retry prompt".to_owned(),
                    name: None,
                    tool_call_id: None,
                    tool_calls: Vec::new(),
                },
                DurableRunMessage {
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
        let path = std::env::temp_dir().join(format!("loom-persistence-{}.db", Uuid::new_v4()));
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
        clone.release_exclusive_writer().unwrap();
        let replacement = FilePersistence::open_exclusive_writer(&path).unwrap();
        drop(replacement);
        drop(clone);
        let replacement = FilePersistence::open_exclusive_writer(&path).unwrap();
        drop(replacement);
        let mut lock_path = path.as_os_str().to_os_string();
        lock_path.push(".loom-owner.lock");
        fs::remove_file(PathBuf::from(lock_path)).unwrap();
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
        let path = std::env::temp_dir().join(format!("loom-persistence-{}.db", Uuid::new_v4()));
        let writer = FilePersistence::open_exclusive_writer(&path).unwrap();
        run_exclusive_writer_probe(&path, true);
        drop(writer);
        run_exclusive_writer_probe(&path, false);
        let mut lock_path = path.as_os_str().to_os_string();
        lock_path.push(".loom-owner.lock");
        fs::remove_file(PathBuf::from(lock_path)).unwrap();
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
        let restored = persistence.load_sessions().unwrap().unwrap();
        assert_eq!(restored, state);
        assert_eq!(
            SessionManager::from_state(restored)
                .unwrap()
                .get(second.id)
                .unwrap(),
            second
        );

        let connection = Connection::open(&path).unwrap();
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
                    role: loom_model::MessageRole::User,
                    content: "large transcript content ".repeat(500),
                    name: None,
                    tool_call_id: None,
                    tool_calls: Vec::new(),
                },
                DurableRunMessage {
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
        assert!(
            content_count >= 8,
            "retained transcript, context checkpoint, tool, filesystem undo, and run-instruction content remain reachable"
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
                    role: loom_model::MessageRole::User,
                    content: "question".to_owned(),
                    name: None,
                    tool_call_id: None,
                    tool_calls: Vec::new(),
                },
                DurableRunMessage {
                    role: loom_model::MessageRole::Assistant,
                    content: "seed".to_owned(),
                    name: None,
                    tool_call_id: None,
                    tool_calls: Vec::new(),
                },
                DurableRunMessage {
                    role: loom_model::MessageRole::Assistant,
                    content: String::new(),
                    name: None,
                    tool_call_id: None,
                    tool_calls: Vec::new(),
                },
                DurableRunMessage {
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
        let fragments_after_mismatch: i64 = Connection::open(&path)
            .unwrap()
            .query_row(
                "SELECT COUNT(*) FROM run_message_fragments WHERE run_id=?1",
                [run_id.as_uuid().as_bytes().as_slice()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(fragments_after_mismatch, 5);

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
        let retained_fragments: i64 = Connection::open(&path)
            .unwrap()
            .query_row(
                "SELECT COUNT(*) FROM run_message_fragments WHERE run_id=?1",
                [run_id.as_uuid().as_bytes().as_slice()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(retained_fragments, 5);
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
        let remaining_fragments: i64 = Connection::open(&path)
            .unwrap()
            .query_row(
                "SELECT COUNT(*) FROM run_message_fragments WHERE run_id=?1",
                [run_id.as_uuid().as_bytes().as_slice()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(remaining_fragments, 0);
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
        let range_plan: String = connection
            .query_row(
                "EXPLAIN QUERY PLAN SELECT byte_offset FROM run_message_fragments
                 WHERE run_id=?1 AND message_ordinal=?2 AND byte_offset<?3
                 ORDER BY byte_offset",
                params![run_id.as_uuid().as_bytes().as_slice(), 2_i64, 6_i64],
                |row| row.get(3),
            )
            .unwrap();
        assert!(
            range_plan.contains("run_message_fragments_by_range"),
            "{range_plan}"
        );
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
            pending_tool_execution: None,
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
                pending_tool_execution: Some(queued_call.clone()),
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
