use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use flate2::{Compression, read::ZlibDecoder, write::ZlibEncoder};
use loom_core::{
    ActivityId, AgentSessionId, AgentSessionSnapshot, AgentSessionState, ApprovalPolicy,
    CheckpointId, ErrorCode, EventSequence, InteractionId, LoomError, RequestId, Result,
    RunAttemptId, RunId, StepId, Timestamp, UsageSnapshot, WorkspaceId, WorkspaceRecord,
};
use loom_model::{ModelId, ProviderHealth, ProviderId, ProviderUsageSummary};
use loom_protocol::{
    AgentActivityData, AgentActivityKind, AgentActivityRecord, AgentActivityStatus,
    AgentExecutionStateRecord, AgentInteractionKind, AgentInteractionRecord,
    AgentInteractionStatus, AgentRunAttemptRecord, AgentRunSnapshot, AgentRunState,
    ApprovalDecision, Checkpoint, CheckpointFile, ServerEventEnvelope, WorkspaceConfig,
    WorkspaceControl,
};
use loom_providers::{ProviderConfig, ProviderUsageKey, UsageLedger};
use loom_session::{SessionManagerState, WorkspaceManagerState};
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use serde::{Serialize, de::DeserializeOwned};
use serde_json::Value;
use sha2::{Digest, Sha256};
use uuid::Uuid;

pub const CURRENT_SCHEMA_VERSION: u32 = 23;
const DATABASE_SCHEMA_VERSION: u32 = 23;
const EXTERNAL_STRING_THRESHOLD: usize = 4096;
const MAX_CONTENT_BYTES: usize = 512 * 1024 * 1024;
const CONTENT_PART_BYTES: usize = 256 * 1024;
const MAX_MESSAGE_FRAGMENT_BYTES: usize = 32 * 1024;
const MAX_CONTENT_RANGE_BYTES: usize =
    loom_protocol::MAX_AGENT_RUN_MESSAGE_CONTENT_RANGE_BYTES as usize;
const MAX_RUN_MESSAGE_PAGE_SIZE: usize = loom_protocol::MAX_AGENT_RUN_MESSAGE_PAGE_SIZE as usize;
const MAX_FEED_EVENT_BYTES: usize = 128 * 1024 * 1024;
const MAX_DURABLE_FEED_BYTES: usize = 16 * 1024 * 1024;
const MAX_IDEMPOTENCY_RECORDS: usize = 1024;
const MAX_IDEMPOTENCY_PAYLOAD_BYTES: usize = 1024 * 1024;

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
    state TEXT NOT NULL CHECK(state IN (
        'planning', 'executing', 'awaiting_approval', 'paused', 'needs_input',
        'evaluating', 'completed', 'failed', 'cancelled'
    )),
    started_at INTEGER NOT NULL CHECK(started_at >= 0),
    updated_at INTEGER NOT NULL CHECK(updated_at >= started_at),
    completed_at INTEGER CHECK(completed_at IS NULL OR completed_at >= started_at),
    snapshot TEXT NOT NULL CHECK(length(snapshot) <= 1048576),
    usage TEXT NOT NULL CHECK(length(usage) <= 16384),
    UNIQUE(run_id, session_id)
) WITHOUT ROWID, STRICT;
CREATE INDEX IF NOT EXISTS runs_by_session_activity
    ON run_summaries(session_id, updated_at DESC, run_id DESC);
CREATE INDEX IF NOT EXISTS runs_by_state_activity
    ON run_summaries(state, updated_at DESC, run_id DESC);
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
    tool_calls TEXT NOT NULL CHECK(length(tool_calls) <= 1048576),
    PRIMARY KEY(run_id, ordinal),
    FOREIGN KEY(run_id, session_id)
        REFERENCES run_summaries(run_id, session_id) ON DELETE CASCADE
) WITHOUT ROWID, STRICT;
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
    approval_policy TEXT NOT NULL CHECK(length(approval_policy) <= 16384),
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
    request_hash BLOB NOT NULL CHECK(length(request_hash) = 32),
    request TEXT NOT NULL CHECK(length(request) <= 1048576),
    response TEXT NOT NULL CHECK(length(response) <= 1048576)
) WITHOUT ROWID, STRICT;
CREATE INDEX IF NOT EXISTS idempotency_expiry
    ON idempotency_records(created_at, request_id);
CREATE TABLE IF NOT EXISTS feed_store_meta (
    singleton INTEGER PRIMARY KEY CHECK(singleton = 1),
    next_sequence INTEGER NOT NULL CHECK(next_sequence >= 0),
    retention_limit INTEGER NOT NULL CHECK(retention_limit >= 0)
) STRICT;
CREATE TABLE IF NOT EXISTS feed_events (
    sequence INTEGER PRIMARY KEY CHECK(sequence > 0),
    session_id BLOB NOT NULL CHECK(length(session_id) = 16),
    payload_codec INTEGER NOT NULL CHECK(payload_codec IN (0, 1)),
    payload BLOB NOT NULL
) STRICT;
CREATE INDEX IF NOT EXISTS feed_events_by_session_sequence
    ON feed_events(session_id, sequence);
CREATE TABLE IF NOT EXISTS section_meta (
    name TEXT PRIMARY KEY NOT NULL,
    schema_version INTEGER NOT NULL
) WITHOUT ROWID, STRICT;
CREATE TABLE IF NOT EXISTS content_blobs (
    hash BLOB PRIMARY KEY NOT NULL CHECK(length(hash) = 32),
    raw_size INTEGER NOT NULL CHECK(raw_size > 0 AND raw_size <= 262144),
    codec INTEGER NOT NULL CHECK(codec IN (0, 1)),
    payload BLOB NOT NULL
) WITHOUT ROWID, STRICT;
CREATE TABLE IF NOT EXISTS content_objects (
    hash BLOB PRIMARY KEY NOT NULL CHECK(length(hash) = 32),
    raw_size INTEGER NOT NULL CHECK(raw_size >= 0 AND raw_size <= 536870912)
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
CREATE TABLE IF NOT EXISTS state_nodes (
    section TEXT NOT NULL REFERENCES section_meta(name) ON DELETE CASCADE,
    path TEXT NOT NULL,
    node_kind INTEGER NOT NULL CHECK(node_kind BETWEEN 0 AND 3),
    scalar BLOB,
    content_hash BLOB REFERENCES content_objects(hash) ON DELETE RESTRICT,
    PRIMARY KEY(section, path),
    CHECK((node_kind = 2 AND scalar IS NOT NULL AND content_hash IS NULL)
       OR (node_kind = 3 AND scalar IS NULL AND content_hash IS NOT NULL)
       OR (node_kind IN (0, 1) AND scalar IS NULL AND content_hash IS NULL))
) WITHOUT ROWID, STRICT;
CREATE INDEX IF NOT EXISTS state_nodes_by_section_kind
    ON state_nodes(section, node_kind);
";

#[derive(Default)]
struct RestoreNode {
    kind: Option<i64>,
    scalar: Option<Vec<u8>>,
    content_hash: Option<Vec<u8>>,
    children: BTreeMap<String, RestoreNode>,
}

fn child_path(parent: &str, segment: &str) -> String {
    if parent.is_empty() {
        format!("/{segment}")
    } else {
        format!("{parent}/{segment}")
    }
}

fn object_segment(key: &str) -> String {
    format!("k{}", key.replace('~', "~0").replace('/', "~1"))
}

fn array_segment(index: usize) -> String {
    format!("a{index}")
}

fn insert_restore_node(
    root: &mut RestoreNode,
    path: &str,
    kind: i64,
    scalar: Option<Vec<u8>>,
    content_hash: Option<Vec<u8>>,
) -> Result<()> {
    let mut current = root;
    if !path.is_empty() {
        for segment in path.trim_start_matches('/').split('/') {
            current = current.children.entry(segment.to_owned()).or_default();
        }
    }
    if current.kind.replace(kind).is_some() || !current.children.is_empty() {
        return Err(LoomError::new(
            ErrorCode::MalformedPayload,
            "persistence contains duplicate or inconsistent state paths",
            false,
        ));
    }
    current.scalar = scalar;
    current.content_hash = content_hash;
    Ok(())
}

fn unescape_object_segment(segment: &str) -> Option<String> {
    let segment = segment.strip_prefix('k')?;
    let mut decoded = String::with_capacity(segment.len());
    let mut chars = segment.chars();
    while let Some(character) = chars.next() {
        if character == '~' {
            match chars.next()? {
                '0' => decoded.push('~'),
                '1' => decoded.push('/'),
                _ => return None,
            }
        } else {
            decoded.push(character);
        }
    }
    Some(decoded)
}

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
    if raw_size <= 0 || raw_size > CONTENT_PART_BYTES as i64 {
        return Err(LoomError::new(
            ErrorCode::MalformedPayload,
            "persisted content part has an invalid size",
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
                        format!("persisted content part is malformed: {error}"),
                        false,
                    )
                })?;
            decoded
        }
        _ => {
            return Err(LoomError::new(
                ErrorCode::MalformedPayload,
                "persisted content part uses an unsupported codec",
                false,
            ));
        }
    };
    if bytes.len() as i64 != raw_size || Sha256::digest(&bytes).as_slice() != hash {
        return Err(LoomError::new(
            ErrorCode::MalformedPayload,
            "persisted content part failed its length or hash check",
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

impl RestoreNode {
    fn into_value(self, connection: &Connection, path: &str) -> Result<Value> {
        match self.kind.ok_or_else(|| {
            LoomError::new(
                ErrorCode::MalformedPayload,
                format!("persistence state path '{path}' has no value"),
                false,
            )
        })? {
            0 => {
                let mut object = serde_json::Map::new();
                for (segment, child) in self.children {
                    let key = unescape_object_segment(&segment).ok_or_else(|| {
                        LoomError::new(
                            ErrorCode::MalformedPayload,
                            "persistence contains an invalid object key",
                            false,
                        )
                    })?;
                    object.insert(
                        key,
                        child.into_value(connection, &child_path(path, &segment))?,
                    );
                }
                Ok(Value::Object(object))
            }
            1 => {
                let mut indexed = Vec::with_capacity(self.children.len());
                for (segment, child) in self.children {
                    let index = segment
                        .strip_prefix('a')
                        .and_then(|value| value.parse::<usize>().ok())
                        .ok_or_else(|| {
                            LoomError::new(
                                ErrorCode::MalformedPayload,
                                "persistence contains an invalid array index",
                                false,
                            )
                        })?;
                    indexed.push((
                        index,
                        child.into_value(connection, &child_path(path, &segment))?,
                    ));
                }
                indexed.sort_by_key(|(index, _)| *index);
                if indexed
                    .iter()
                    .enumerate()
                    .any(|(expected, (actual, _))| expected != *actual)
                {
                    return Err(LoomError::new(
                        ErrorCode::MalformedPayload,
                        "persistence contains a sparse array",
                        false,
                    ));
                }
                Ok(Value::Array(
                    indexed.into_iter().map(|(_, value)| value).collect(),
                ))
            }
            2 => self
                .scalar
                .as_deref()
                .ok_or_else(|| {
                    LoomError::new(
                        ErrorCode::MalformedPayload,
                        "persistence scalar payload is missing",
                        false,
                    )
                })
                .and_then(|payload| {
                    serde_json::from_slice(payload).map_err(|error| {
                        LoomError::new(
                            ErrorCode::MalformedPayload,
                            format!("persistence contains malformed JSON data: {error}"),
                            false,
                        )
                    })
                }),
            3 => {
                let hash = self.content_hash.ok_or_else(|| {
                    LoomError::new(
                        ErrorCode::MalformedPayload,
                        "persistence content reference is missing",
                        false,
                    )
                })?;
                Ok(Value::String(decode_content(connection, &hash)?))
            }
            _ => Err(LoomError::new(
                ErrorCode::MalformedPayload,
                "persistence contains an unknown node kind",
                false,
            )),
        }
    }
}

#[derive(Clone, Debug)]
pub struct FilePersistence {
    path: PathBuf,
}

#[derive(Clone, Debug)]
pub struct DurableFeedState {
    pub next_sequence: EventSequence,
    pub retention_limit: usize,
    pub events: Vec<ServerEventEnvelope>,
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
pub struct DurableRunMessage {
    pub role: loom_model::MessageRole,
    pub content: String,
    pub name: Option<String>,
    pub tool_call_id: Option<loom_core::ToolCallId>,
    pub tool_calls: Vec<loom_model::ToolCall>,
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

#[derive(Clone, Debug)]
pub struct DurableFilesystemRecord {
    pub session_id: AgentSessionId,
    pub root: String,
    pub control: WorkspaceControl,
    pub checkpoints: Vec<Checkpoint>,
    pub payload: Value,
}

pub struct DurableStateWrite<'a> {
    pub schema_version: u32,
    pub sessions: &'a SessionManagerState,
    pub workspaces: Option<&'a WorkspaceManagerState>,
    pub settings: Option<&'a DurableSessionSettings>,
    pub workspace_configs: Option<&'a BTreeMap<WorkspaceId, WorkspaceConfig>>,
    pub providers: Option<&'a DurableProviderState>,
    pub usage: Option<&'a UsageLedger>,
    pub idempotency: Option<&'a BTreeMap<RequestId, DurableIdempotencyRecord>>,
    pub run_summaries: Option<&'a BTreeMap<RunId, DurableRunSummary>>,
    pub run_messages: Option<&'a BTreeMap<RunId, Vec<DurableRunMessage>>>,
    pub run_activities: Option<&'a DurableRunActivities>,
    pub filesystem_records: Option<&'a [DurableFilesystemRecord]>,
    pub records: &'a [(String, Value)],
    pub feed: Option<&'a DurableFeedState>,
    pub sections: &'a [(&'a str, Value)],
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

        Ok(Self { path })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn exists(&self) -> bool {
        self.path.is_file()
    }

    pub fn load_section<T: DeserializeOwned>(
        &self,
        section: &str,
        expected_version: u32,
    ) -> Result<Option<T>> {
        if !self.path.exists() {
            return Ok(None);
        }
        let connection = self.connection()?;
        let row = connection
            .query_row(
                "SELECT schema_version FROM section_meta WHERE name = ?1",
                [section],
                |row| row.get::<_, u32>(0),
            )
            .optional()
            .map_err(|error| {
                persistence_error(format!("could not read section '{section}': {error}"), true)
            })?;
        let Some(schema_version) = row else {
            return Ok(None);
        };
        if schema_version != expected_version {
            return Err(LoomError::new(
                ErrorCode::MalformedPayload,
                format!(
                    "unsupported persistence schema version {schema_version} (expected {expected_version})"
                ),
                false,
            ));
        }
        let mut root = RestoreNode::default();
        {
            let mut statement = connection
                .prepare(
                    "SELECT path, node_kind, scalar, content_hash
                     FROM state_nodes WHERE section = ?1 ORDER BY path",
                )
                .map_err(|error| {
                    persistence_error(
                        format!("could not prepare section '{section}': {error}"),
                        true,
                    )
                })?;
            let rows = statement
                .query_map([section], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, Option<Vec<u8>>>(2)?,
                        row.get::<_, Option<Vec<u8>>>(3)?,
                    ))
                })
                .map_err(|error| {
                    persistence_error(format!("could not read section '{section}': {error}"), true)
                })?;
            for row in rows {
                let (path, kind, scalar, content_hash) = row.map_err(|error| {
                    persistence_error(format!("could not read section '{section}': {error}"), true)
                })?;
                insert_restore_node(&mut root, &path, kind, scalar, content_hash)?;
            }
        }
        root.into_value(&connection, "")
            .and_then(|value| {
                serde_json::from_value(value).map_err(|error| {
                    LoomError::new(
                        ErrorCode::MalformedPayload,
                        format!("persistence section '{section}' has invalid data: {error}"),
                        false,
                    )
                })
            })
            .map(Some)
    }

    /// Loads one subtree from a section without decoding sibling records.
    pub fn load_section_path<T: DeserializeOwned>(
        &self,
        section: &str,
        path: &str,
        expected_version: u32,
    ) -> Result<Option<T>> {
        if !path.is_empty() && !path.starts_with('/') {
            return Err(LoomError::invalid_request(
                "persistence subtree path must be empty or start with '/'",
            ));
        }
        if !self.path.exists() {
            return Ok(None);
        }
        let connection = self.connection()?;
        let version = connection
            .query_row(
                "SELECT schema_version FROM section_meta WHERE name=?1",
                [section],
                |row| row.get::<_, u32>(0),
            )
            .optional()
            .map_err(|error| {
                persistence_error(
                    format!("could not inspect section '{section}': {error}"),
                    true,
                )
            })?;
        let Some(version) = version else {
            return Ok(None);
        };
        if version != expected_version {
            return Err(LoomError::new(
                ErrorCode::MalformedPayload,
                format!(
                    "unsupported persistence schema version {version} (expected {expected_version})"
                ),
                false,
            ));
        }
        let mut root = RestoreNode::default();
        let mut found = false;
        {
            let mut statement = connection
                .prepare(
                    "SELECT path, node_kind, scalar, content_hash FROM state_nodes
                     WHERE section=?1 AND (path=?2 OR substr(path, 1, length(?2)+1)=?2 || '/')
                     ORDER BY path",
                )
                .map_err(|error| {
                    persistence_error(
                        format!("could not prepare section '{section}': {error}"),
                        true,
                    )
                })?;
            let rows = statement
                .query_map(params![section, path], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, Option<Vec<u8>>>(2)?,
                        row.get::<_, Option<Vec<u8>>>(3)?,
                    ))
                })
                .map_err(|error| {
                    persistence_error(format!("could not read section '{section}': {error}"), true)
                })?;
            for row in rows {
                let (stored_path, kind, scalar, content_hash) = row.map_err(|error| {
                    persistence_error(format!("could not read section '{section}': {error}"), true)
                })?;
                found = true;
                let relative_path = &stored_path[path.len()..];
                insert_restore_node(&mut root, relative_path, kind, scalar, content_hash)?;
            }
        }
        if !found {
            return Ok(None);
        }
        let value = root.into_value(&connection, path)?;
        serde_json::from_value(value).map(Some).map_err(|error| {
            LoomError::new(
                ErrorCode::MalformedPayload,
                format!("persistence subtree '{section}{path}' has invalid data: {error}"),
                false,
            )
        })
    }

    /// Lists section names in a namespace, such as individually persisted runs.
    pub fn list_sections_with_prefix(&self, prefix: &str) -> Result<Vec<String>> {
        if !self.path.exists() {
            return Ok(Vec::new());
        }
        let connection = self.connection()?;
        let mut statement = connection
            .prepare(
                "SELECT name FROM section_meta WHERE substr(name, 1, length(?1))=?1 ORDER BY name",
            )
            .map_err(|error| {
                persistence_error(
                    format!("could not list persistence sections: {error}"),
                    true,
                )
            })?;
        let rows = statement
            .query_map([prefix], |row| row.get(0))
            .map_err(|error| {
                persistence_error(
                    format!("could not list persistence sections: {error}"),
                    true,
                )
            })?;
        rows.collect::<std::result::Result<Vec<String>, _>>()
            .map_err(|error| {
                persistence_error(
                    format!("could not list persistence sections: {error}"),
                    true,
                )
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
            .prepare("SELECT session_id, approval_policy, auto_approve_actions FROM session_settings ORDER BY session_id")
            .map_err(|error| {
                persistence_error(format!("could not prepare session settings: {error}"), true)
            })?;
        let rows = statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, Vec<u8>>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?,
                ))
            })
            .map_err(|error| {
                persistence_error(format!("could not read session settings: {error}"), true)
            })?;
        let mut settings = DurableSessionSettings::default();
        for row in rows {
            let (id, policy, auto_approve) = row.map_err(|error| {
                persistence_error(format!("could not read session settings: {error}"), true)
            })?;
            let id = AgentSessionId::from_uuid(decode_uuid(&id, "session id")?);
            let policy = serde_json::from_str(&policy).map_err(|error| {
                LoomError::new(
                    ErrorCode::MalformedPayload,
                    format!("persisted approval policy is malformed: {error}"),
                    false,
                )
            })?;
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
                "SELECT request_id, created_at, request_hash, request, response
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
                    row.get::<_, Vec<u8>>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                ))
            })
            .map_err(|error| {
                persistence_error(format!("could not read idempotency records: {error}"), true)
            })?;
        let mut records = BTreeMap::new();
        for row in rows {
            let (id, created_at, request_hash, request, response) = row.map_err(|error| {
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
                    request,
                    response,
                },
            );
        }
        Ok(records)
    }

    /// Loads indexed run summaries without reading runtime details or transcripts.
    pub fn load_run_summaries(&self) -> Result<BTreeMap<RunId, DurableRunSummary>> {
        if !self.path.exists() {
            return Ok(BTreeMap::new());
        }
        let connection = self.connection()?;
        let mut statement = connection
            .prepare(
                "SELECT run_id, session_id, state, started_at, updated_at, completed_at,
                        snapshot, usage
                 FROM run_summaries ORDER BY updated_at DESC, run_id DESC",
            )
            .map_err(|error| {
                persistence_error(format!("could not prepare run summaries: {error}"), true)
            })?;
        let rows = statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, Vec<u8>>(0)?,
                    row.get::<_, Vec<u8>>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, i64>(4)?,
                    row.get::<_, Option<i64>>(5)?,
                    row.get::<_, String>(6)?,
                    row.get::<_, String>(7)?,
                ))
            })
            .map_err(|error| {
                persistence_error(format!("could not read run summaries: {error}"), true)
            })?;
        let mut summaries = BTreeMap::new();
        for row in rows {
            let (run_id, session_id, state, started, updated, completed, snapshot, usage) = row
                .map_err(|error| {
                    persistence_error(format!("could not read run summaries: {error}"), true)
                })?;
            let run_id = RunId::from_uuid(decode_uuid(&run_id, "run id")?);
            let session_id = AgentSessionId::from_uuid(decode_uuid(&session_id, "run session id")?);
            let snapshot: AgentRunSnapshot = serde_json::from_str(&snapshot).map_err(|error| {
                LoomError::new(
                    ErrorCode::MalformedPayload,
                    format!("persisted run summary is malformed: {error}"),
                    false,
                )
            })?;
            let usage: UsageSnapshot = serde_json::from_str(&usage).map_err(|error| {
                LoomError::new(
                    ErrorCode::MalformedPayload,
                    format!("persisted run usage is malformed: {error}"),
                    false,
                )
            })?;
            let completed = completed.map(decode_timestamp).transpose()?;
            if snapshot.id != run_id
                || snapshot.session_id != session_id
                || run_state_name(snapshot.state) != state
                || snapshot.started_at != decode_timestamp(started)?
                || snapshot.updated_at != decode_timestamp(updated)?
                || snapshot.completed_at != completed
            {
                return Err(LoomError::new(
                    ErrorCode::MalformedPayload,
                    "persisted run summary columns do not match its payload",
                    false,
                ));
            }
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
        Ok(summaries)
    }

    /// Loads a run's typed attempt history independently of its runtime section.
    pub fn load_run_attempts(&self, run_id: RunId) -> Result<Vec<AgentRunAttemptRecord>> {
        if !self.path.exists() {
            return Ok(Vec::new());
        }
        let connection = self.connection()?;
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
    /// its summary and runtime section.
    pub fn load_run_interactions(&self, run_id: RunId) -> Result<Vec<AgentInteractionRecord>> {
        if !self.path.exists() {
            return Ok(Vec::new());
        }
        let connection = self.connection()?;
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
        let mut statement = connection
            .prepare(
                "SELECT ordinal, role, content_hash, name, tool_call_id, tool_calls
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
                    row.get::<_, String>(5)?,
                ))
            })
            .map_err(|error| {
                persistence_error(format!("could not read run messages: {error}"), true)
            })?;
        rows.map(|row| {
            let (ordinal, role, content_hash, name, tool_call_id, tool_calls) =
                row.map_err(|error| {
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
            let tool_calls = serde_json::from_str(&tool_calls).map_err(|error| {
                LoomError::new(
                    ErrorCode::MalformedPayload,
                    format!("persisted tool calls are malformed: {error}"),
                    false,
                )
            })?;
            let tool_call_id = decode_optional_tool_call_id(tool_call_id)?;
            Ok(DurableRunMessage {
                role,
                content,
                name,
                tool_call_id,
                tool_calls,
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
            let data = decode_content(&connection, &data_hash)?;
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
            activities.push(AgentActivityRecord {
                id: ActivityId::from_uuid(decode_uuid(&activity_id, "activity id")?),
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
                        m.name, m.tool_call_id, m.tool_calls
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
                        row.get::<_, String>(5)?,
                    ))
                },
            )
            .map_err(|error| {
                persistence_error(format!("could not query run message page: {error}"), true)
            })?;
        rows.map(|row| {
            let (ordinal, role, content_bytes, name, tool_call_id, tool_calls) =
                row.map_err(|error| {
                    persistence_error(format!("could not read run message header: {error}"), true)
                })?;
            let tool_calls = serde_json::from_str(&tool_calls).map_err(|error| {
                LoomError::new(
                    ErrorCode::MalformedPayload,
                    format!("persisted tool calls are malformed: {error}"),
                    false,
                )
            })?;
            Ok(DurableRunMessageHeader {
                ordinal: u64::try_from(ordinal).map_err(|_| {
                    LoomError::new(
                        ErrorCode::MalformedPayload,
                        "persisted message ordinal is negative",
                        false,
                    )
                })?,
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
                tool_calls,
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
                    run_id, session_id, ordinal, role, content_hash, name, tool_call_id, tool_calls
                 ) VALUES (?1, ?2, ?3, 'assistant', NULL, NULL, NULL, '[]')
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
        let payload: Value = serde_json::from_slice(&raw).map_err(|error| {
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
        Ok(Some(DurableFilesystemRecord {
            session_id,
            root,
            control,
            checkpoints,
            payload,
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
        let encoded_bytes: i64 = connection
            .query_row(
                "SELECT COALESCE(SUM(length(payload)), 0) FROM feed_events",
                [],
                |row| row.get(0),
            )
            .map_err(|error| {
                persistence_error(format!("could not validate event feed size: {error}"), true)
            })?;
        if encoded_bytes < 0 || encoded_bytes > MAX_DURABLE_FEED_BYTES as i64 {
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
        if events.len() > retention_limit
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
        }))
    }

    /// Persists typed session rows and the remaining bounded state snapshot atomically.
    pub fn save_state_with_sessions(
        &self,
        schema_version: u32,
        sessions: &SessionManagerState,
        sections: &[(&str, Value)],
    ) -> Result<()> {
        self.save_state_with_sessions_and_entities(schema_version, sessions, &[], sections)
    }

    /// Persists session rows, individual entity records, and auxiliary state atomically.
    pub fn save_state_with_sessions_and_entities(
        &self,
        schema_version: u32,
        sessions: &SessionManagerState,
        records: &[(String, Value)],
        sections: &[(&str, Value)],
    ) -> Result<()> {
        self.save_state_with_sessions_entities_and_feed(
            schema_version,
            sessions,
            records,
            None,
            sections,
        )
    }

    /// Persists catalog rows, entity records, the event feed, and auxiliary state atomically.
    pub fn save_state_with_sessions_entities_and_feed(
        &self,
        schema_version: u32,
        sessions: &SessionManagerState,
        records: &[(String, Value)],
        feed: Option<&DurableFeedState>,
        sections: &[(&str, Value)],
    ) -> Result<()> {
        self.save_state(DurableStateWrite {
            schema_version,
            sessions,
            workspaces: None,
            settings: None,
            workspace_configs: None,
            providers: None,
            usage: None,
            idempotency: None,
            run_summaries: None,
            run_messages: None,
            run_activities: None,
            filesystem_records: None,
            records,
            feed,
            sections,
        })
    }

    /// Persists typed session and workspace catalogs with entity records atomically.
    pub fn save_state_with_catalogs_entities_and_feed(
        &self,
        schema_version: u32,
        sessions: &SessionManagerState,
        workspaces: &WorkspaceManagerState,
        records: &[(String, Value)],
        feed: Option<&DurableFeedState>,
        sections: &[(&str, Value)],
    ) -> Result<()> {
        self.save_state(DurableStateWrite {
            schema_version,
            sessions,
            workspaces: Some(workspaces),
            settings: None,
            workspace_configs: None,
            providers: None,
            usage: None,
            idempotency: None,
            run_summaries: None,
            run_messages: None,
            run_activities: None,
            filesystem_records: None,
            records,
            feed,
            sections,
        })
    }

    /// Persists catalogs and settings with entity records and the feed atomically.
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
        if let Some(run_activities) = write.run_activities {
            save_run_activity_rows(&transaction, run_activities)?;
        }
        if let Some(run_messages) = write.run_messages {
            save_run_message_rows(&transaction, run_messages)?;
        }
        if let Some(filesystems) = write.filesystem_records {
            save_filesystem_records(&transaction, filesystems)?;
        }
        if let Some(feed) = write.feed {
            save_feed_rows(&transaction, feed)?;
        }
        for (name, value) in write.sections {
            save_section_nodes(&transaction, name, write.schema_version, value)?;
        }
        for (name, value) in write.records {
            if !name.starts_with("run:") {
                return Err(LoomError::invalid_request(
                    "individually stored run detail must use the run section prefix",
                ));
            }
            save_section_nodes(&transaction, name, write.schema_version, value)?;
        }
        transaction.commit().map_err(|error| {
            persistence_error(
                format!("could not commit persistence transaction: {error}"),
                true,
            )
        })
    }

    pub fn save_sections(&self, schema_version: u32, sections: &[(&str, Value)]) -> Result<()> {
        let connection = self.connection_for_write()?;
        let transaction = connection.unchecked_transaction().map_err(|error| {
            persistence_error(
                format!("could not begin persistence transaction: {error}"),
                true,
            )
        })?;
        for (name, value) in sections {
            save_section_nodes(&transaction, name, schema_version, value)?;
        }
        transaction.commit().map_err(|error| {
            persistence_error(
                format!("could not commit persistence transaction: {error}"),
                true,
            )
        })
    }

    fn connection(&self) -> Result<Connection> {
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
        Ok(connection)
    }

    fn connection_for_write(&self) -> Result<Connection> {
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
        self.connection()
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

    // Inspect before changing persistent SQLite settings. In particular, opening
    // a database from the old section format must not even switch its journal
    // mode; this release deliberately starts with an empty database only.
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

fn save_section_nodes(
    transaction: &Transaction<'_>,
    name: &str,
    schema_version: u32,
    value: &Value,
) -> Result<()> {
    if name.is_empty() {
        return Err(LoomError::invalid_request(
            "persistence section name must not be empty",
        ));
    }
    transaction
        .execute(
            "INSERT INTO section_meta(name, schema_version) VALUES (?1, ?2)
             ON CONFLICT(name) DO UPDATE SET schema_version=excluded.schema_version",
            params![name, schema_version],
        )
        .map_err(|error| {
            persistence_error(format!("could not write section '{name}': {error}"), true)
        })?;
    transaction
        .execute_batch("CREATE TEMP TABLE IF NOT EXISTS _loom_wanted_nodes (path TEXT PRIMARY KEY) WITHOUT ROWID;")
        .map_err(|error| persistence_error(format!("could not stage section '{name}': {error}"), true))?;
    transaction
        .execute("DELETE FROM _loom_wanted_nodes", [])
        .map_err(|error| {
            persistence_error(format!("could not stage section '{name}': {error}"), true)
        })?;

    let mut nodes = Vec::new();
    flatten_value(value, "", &mut nodes)?;
    for (path, kind, scalar, content) in nodes {
        let content_hash = match content {
            Some(content) => Some(store_content(transaction, &content)?),
            None => None,
        };
        transaction
            .execute("INSERT INTO _loom_wanted_nodes(path) VALUES (?1)", [&path])
            .map_err(|error| {
                persistence_error(format!("could not stage section '{name}': {error}"), true)
            })?;
        transaction
            .execute(
                "INSERT INTO state_nodes(section, path, node_kind, scalar, content_hash)
                 VALUES (?1, ?2, ?3, ?4, ?5)
                 ON CONFLICT(section, path) DO UPDATE SET
                    node_kind=excluded.node_kind, scalar=excluded.scalar, content_hash=excluded.content_hash
                 WHERE state_nodes.node_kind IS NOT excluded.node_kind
                    OR state_nodes.scalar IS NOT excluded.scalar
                    OR state_nodes.content_hash IS NOT excluded.content_hash",
                params![name, path, kind, scalar, content_hash],
            )
            .map_err(|error| persistence_error(format!("could not write section '{name}': {error}"), true))?;
    }
    transaction
        .execute(
            "DELETE FROM state_nodes
             WHERE section=?1 AND NOT EXISTS (
                 SELECT 1 FROM _loom_wanted_nodes wanted WHERE wanted.path=state_nodes.path
             )",
            [name],
        )
        .map_err(|error| {
            persistence_error(format!("could not prune section '{name}': {error}"), true)
        })?;
    collect_unused_content(transaction)?;
    Ok(())
}

type EncodedNode = (String, i64, Option<Vec<u8>>, Option<Vec<u8>>);

fn flatten_value(value: &Value, path: &str, nodes: &mut Vec<EncodedNode>) -> Result<()> {
    match value {
        Value::Object(object) => {
            nodes.push((path.to_owned(), 0, None, None));
            for (key, value) in object {
                flatten_value(value, &child_path(path, &object_segment(key)), nodes)?;
            }
        }
        Value::Array(array) => {
            nodes.push((path.to_owned(), 1, None, None));
            for (index, value) in array.iter().enumerate() {
                flatten_value(value, &child_path(path, &array_segment(index)), nodes)?;
            }
        }
        Value::String(value) if value.len() >= EXTERNAL_STRING_THRESHOLD => {
            if value.len() > MAX_CONTENT_BYTES {
                return Err(LoomError::new(
                    ErrorCode::Persistence,
                    "state text exceeds the maximum supported size",
                    false,
                ));
            }
            nodes.push((path.to_owned(), 3, None, Some(value.as_bytes().to_vec())));
        }
        _ => {
            let scalar = serde_json::to_vec(value).map_err(|error| {
                persistence_error(
                    format!("could not encode persistence value: {error}"),
                    false,
                )
            })?;
            nodes.push((path.to_owned(), 2, Some(scalar), None));
        }
    }
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
    transaction
        .execute(
            "INSERT INTO content_objects(hash, raw_size) VALUES (?1, ?2)",
            params![hash, content.len() as i64],
        )
        .map_err(|error| {
            persistence_error(format!("could not store content metadata: {error}"), true)
        })?;
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

fn collect_unused_content(transaction: &Transaction<'_>) -> Result<()> {
    transaction
        .execute(
            "DELETE FROM content_objects
             WHERE NOT EXISTS (
                SELECT 1 FROM state_nodes WHERE state_nodes.content_hash=content_objects.hash
             ) AND NOT EXISTS (
                SELECT 1 FROM checkpoint_files
                WHERE checkpoint_files.content_hash=content_objects.hash
             ) AND NOT EXISTS (
                SELECT 1 FROM run_messages
                WHERE run_messages.content_hash=content_objects.hash
             ) AND NOT EXISTS (
                SELECT 1 FROM run_message_fragments
                WHERE run_message_fragments.content_hash=content_objects.hash
             ) AND NOT EXISTS (
                SELECT 1 FROM run_activities
                WHERE run_activities.data_hash=content_objects.hash
             )",
            [],
        )
        .map_err(|error| {
            persistence_error(
                format!("could not collect unused content metadata: {error}"),
                true,
            )
        })?;
    transaction
        .execute(
            "DELETE FROM content_blobs
             WHERE NOT EXISTS (
                 SELECT 1 FROM content_parts WHERE content_parts.blob_hash=content_blobs.hash
             )",
            [],
        )
        .map_err(|error| {
            persistence_error(
                format!("could not collect unused content parts: {error}"),
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

fn save_run_message_rows(
    transaction: &Transaction<'_>,
    messages_by_run: &BTreeMap<RunId, Vec<DurableRunMessage>>,
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
            let tool_calls = serde_json::to_string(&message.tool_calls).map_err(|error| {
                persistence_error(
                    format!("could not encode run message tool calls: {error}"),
                    false,
                )
            })?;
            if tool_calls.len() > 1024 * 1024 {
                return Err(LoomError::new(
                    ErrorCode::Persistence,
                    "run message tool calls exceed the maximum supported size",
                    false,
                ));
            }
            let run_id_bytes = run_id.as_uuid().as_bytes();
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
            transaction
                .execute(
                    "INSERT INTO run_messages(run_id, session_id, ordinal, role, content_hash,
                    name, tool_call_id, tool_calls)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
                 ON CONFLICT(run_id, ordinal) DO UPDATE SET
                    session_id=excluded.session_id, role=excluded.role,
                    content_hash=excluded.content_hash, name=excluded.name,
                    tool_call_id=excluded.tool_call_id, tool_calls=excluded.tool_calls
                 WHERE run_messages.session_id IS NOT excluded.session_id
                    OR run_messages.role IS NOT excluded.role
                    OR run_messages.content_hash IS NOT excluded.content_hash
                    OR run_messages.name IS NOT excluded.name
                    OR run_messages.tool_call_id IS NOT excluded.tool_call_id
                    OR run_messages.tool_calls IS NOT excluded.tool_calls",
                    params![
                        run_id_bytes.as_slice(),
                        session_id,
                        ordinal,
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
    collect_unused_content(transaction)
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
        let policy = serde_json::to_string(&policy).map_err(|error| {
            persistence_error(format!("could not encode approval policy: {error}"), false)
        })?;
        if policy.len() > 16_384 {
            return Err(LoomError::new(
                ErrorCode::Persistence,
                "approval policy exceeds the maximum supported size",
                false,
            ));
        }
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
                "INSERT INTO session_settings(session_id, approval_policy, auto_approve_actions)
                 VALUES (?1, ?2, ?3)
                 ON CONFLICT(session_id) DO UPDATE SET
                    approval_policy=excluded.approval_policy,
                    auto_approve_actions=excluded.auto_approve_actions
                 WHERE session_settings.approval_policy IS NOT excluded.approval_policy
                    OR session_settings.auto_approve_actions IS NOT excluded.auto_approve_actions",
                params![session_id.as_slice(), policy, auto_approve],
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
    if records.len() > MAX_IDEMPOTENCY_RECORDS {
        return Err(LoomError::new(
            ErrorCode::Persistence,
            "idempotency cache exceeds its configured record limit",
            false,
        ));
    }
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
                    request_id, created_at, request_hash, request, response
                 ) VALUES (?1, ?2, ?3, ?4, ?5)
                 ON CONFLICT(request_id) DO UPDATE SET
                    created_at=excluded.created_at,
                    request_hash=excluded.request_hash,
                    request=excluded.request,
                    response=excluded.response
                 WHERE idempotency_records.created_at IS NOT excluded.created_at
                    OR idempotency_records.request_hash IS NOT excluded.request_hash
                    OR idempotency_records.request IS NOT excluded.request
                    OR idempotency_records.response IS NOT excluded.response",
                params![
                    id_bytes.as_slice(),
                    encode_timestamp(record.created_at)?,
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

fn save_run_summary_rows(
    transaction: &Transaction<'_>,
    summaries: &BTreeMap<RunId, DurableRunSummary>,
) -> Result<()> {
    transaction
        .execute_batch(
            "CREATE TEMP TABLE IF NOT EXISTS _loom_wanted_runs (
                run_id BLOB PRIMARY KEY NOT NULL
             ) WITHOUT ROWID, STRICT;
             DELETE FROM _loom_wanted_runs;",
        )
        .map_err(|error| {
            persistence_error(format!("could not stage run summaries: {error}"), true)
        })?;
    for (run_id, summary) in summaries {
        if summary.snapshot.id != *run_id {
            return Err(LoomError::new(
                ErrorCode::MalformedPayload,
                "run summary key does not match its run id",
                false,
            ));
        }
        let snapshot = serde_json::to_string(&summary.snapshot).map_err(|error| {
            persistence_error(format!("could not encode run summary: {error}"), false)
        })?;
        let usage = serde_json::to_string(&summary.usage).map_err(|error| {
            persistence_error(format!("could not encode run usage: {error}"), false)
        })?;
        if snapshot.len() > 1024 * 1024 || usage.len() > 16 * 1024 {
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
                "INSERT INTO _loom_wanted_runs(run_id) VALUES (?1)",
                [run_id_bytes.as_slice()],
            )
            .map_err(|error| {
                persistence_error(
                    format!("could not stage run summary {run_id}: {error}"),
                    true,
                )
            })?;
        transaction
            .execute(
                "INSERT INTO run_summaries(
                    run_id, session_id, state, started_at, updated_at, completed_at, snapshot, usage
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
                 ON CONFLICT(run_id) DO UPDATE SET
                    session_id=excluded.session_id,
                    state=excluded.state,
                    started_at=excluded.started_at,
                    updated_at=excluded.updated_at,
                    completed_at=excluded.completed_at,
                    snapshot=excluded.snapshot,
                    usage=excluded.usage
                 WHERE run_summaries.session_id IS NOT excluded.session_id
                    OR run_summaries.state IS NOT excluded.state
                    OR run_summaries.started_at IS NOT excluded.started_at
                    OR run_summaries.updated_at IS NOT excluded.updated_at
                    OR run_summaries.completed_at IS NOT excluded.completed_at
                    OR run_summaries.snapshot IS NOT excluded.snapshot
                    OR run_summaries.usage IS NOT excluded.usage",
                params![
                    run_id_bytes.as_slice(),
                    session_id_bytes.as_slice(),
                    run_state_name(summary.snapshot.state),
                    encode_timestamp(summary.snapshot.started_at)?,
                    encode_timestamp(summary.snapshot.updated_at)?,
                    summary
                        .snapshot
                        .completed_at
                        .map(encode_timestamp)
                        .transpose()?,
                    snapshot,
                    usage,
                ],
            )
            .map_err(|error| {
                persistence_error(
                    format!("could not save run summary {run_id}: {error}"),
                    true,
                )
            })?;
    }
    save_run_interaction_rows(transaction, summaries)?;
    transaction
        .execute(
            "DELETE FROM run_summaries
             WHERE NOT EXISTS (
                SELECT 1 FROM _loom_wanted_runs wanted
                WHERE wanted.run_id=run_summaries.run_id
             )",
            [],
        )
        .map_err(|error| {
            persistence_error(format!("could not prune run summaries: {error}"), true)
        })?;
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

            let data = serde_json::to_vec(&activity.data).map_err(|error| {
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
    collect_unused_content(transaction)
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
        if filesystem.get("root").and_then(Value::as_str) != Some(record.root.as_str())
            || filesystem.get("control").and_then(Value::as_str)
                != Some(workspace_control_name(record.control))
            || filesystem.get("session_id").and_then(Value::as_str)
                != Some(record.session_id.to_string().as_str())
            || filesystem
                .get("checkpoints")
                .and_then(Value::as_array)
                .is_none_or(|checkpoints| !checkpoints.is_empty())
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
        save_checkpoint_rows(transaction, record.session_id, &record.checkpoints)?;
    }
    collect_unused_content(transaction)?;
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
                "INSERT INTO feed_events(sequence, session_id, payload_codec, payload)
                 VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(sequence) DO NOTHING",
                params![
                    sequence,
                    event.session_id.as_uuid().as_bytes().as_slice(),
                    payload_codec,
                    payload
                ],
            )
            .map_err(|error| {
                persistence_error(format!("could not save event feed entry: {error}"), true)
            })?;
    }
    transaction
        .execute(
            "INSERT INTO feed_store_meta(singleton, next_sequence, retention_limit)
             VALUES (1, ?1, ?2)
             ON CONFLICT(singleton) DO UPDATE SET
                next_sequence=excluded.next_sequence,
                retention_limit=excluded.retention_limit
             WHERE feed_store_meta.next_sequence IS NOT excluded.next_sequence
                OR feed_store_meta.retention_limit IS NOT excluded.retention_limit",
            params![next_sequence, retention_limit],
        )
        .map_err(|error| {
            persistence_error(format!("could not save event feed cursor: {error}"), true)
        })?;
    transaction
        .execute(
            "WITH ranked AS (
                SELECT sequence,
                       SUM(length(payload)) OVER (ORDER BY sequence DESC) AS retained_bytes
                FROM feed_events
             )
             DELETE FROM feed_events
             WHERE sequence IN (SELECT sequence FROM ranked WHERE retained_bytes > ?1)
                OR sequence NOT IN (
                    SELECT sequence FROM feed_events ORDER BY sequence DESC LIMIT ?2
                )",
            params![MAX_DURABLE_FEED_BYTES as i64, retention_limit],
        )
        .map_err(|error| persistence_error(format!("could not prune event feed: {error}"), true))?;
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
    use uuid::Uuid;

    #[derive(Debug, Deserialize, PartialEq, Serialize)]
    struct Fixture {
        value: String,
    }

    #[test]
    fn file_store_round_trips_section_data_atomically() {
        let path = std::env::temp_dir().join(format!("loom-persistence-{}.db", Uuid::new_v4()));
        let store = FilePersistence::open(&path).unwrap();
        store
            .save_sections(
                CURRENT_SCHEMA_VERSION,
                &[("state", serde_json::json!({"value": "durable"}))],
            )
            .unwrap();
        assert_eq!(
            store
                .load_section::<Fixture>("state", CURRENT_SCHEMA_VERSION)
                .unwrap()
                .unwrap(),
            Fixture {
                value: "durable".to_owned()
            }
        );
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn non_sqlite_file_is_rejected_without_migration() {
        let path = std::env::temp_dir().join(format!("loom-persistence-{}.db", Uuid::new_v4()));
        fs::write(&path, b"{not json").unwrap();
        let store = FilePersistence::open(&path).unwrap();
        let error = store
            .save_sections(CURRENT_SCHEMA_VERSION, &[("state", serde_json::json!({}))])
            .unwrap_err();
        assert_eq!(error.code, ErrorCode::Persistence);
        assert!(!path.with_extension("json.legacy").exists());
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn file_store_reports_missing_schema_and_malformed_sections() {
        let path = std::env::temp_dir().join(format!("loom-persistence-{}.db", Uuid::new_v4()));
        let store = FilePersistence::open(&path).unwrap();
        assert!(
            store
                .load_section::<Fixture>("missing", 1)
                .unwrap()
                .is_none()
        );
        assert!(!store.exists());
        store
            .save_sections(1, &[("state", serde_json::json!({"value": "old"}))])
            .unwrap();
        assert_eq!(
            store.load_section::<Fixture>("state", 2).unwrap_err().code,
            ErrorCode::MalformedPayload
        );
        let connection = Connection::open(&path).unwrap();
        connection
            .execute(
                "UPDATE state_nodes SET scalar = ?1 WHERE section = 'state' AND path = '/kvalue'",
                [b"invalid json".as_slice()],
            )
            .unwrap();
        assert_eq!(
            store.load_section::<Fixture>("state", 1).unwrap_err().code,
            ErrorCode::MalformedPayload
        );
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
    fn sections_are_updated_without_rewriting_other_sections() {
        let path = std::env::temp_dir().join(format!("loom-persistence-{}.db", Uuid::new_v4()));
        let store = FilePersistence::open(&path).unwrap();
        store
            .save_sections(
                CURRENT_SCHEMA_VERSION,
                &[
                    ("sessions", serde_json::json!({"count": 1})),
                    ("journal", serde_json::json!({"events": 3})),
                ],
            )
            .unwrap();
        store
            .save_sections(
                CURRENT_SCHEMA_VERSION,
                &[("sessions", serde_json::json!({"count": 2}))],
            )
            .unwrap();
        assert_eq!(
            store
                .load_section::<serde_json::Value>("journal", CURRENT_SCHEMA_VERSION)
                .unwrap(),
            Some(serde_json::json!({"events": 3}))
        );
        assert_eq!(
            store
                .load_section::<serde_json::Value>("sessions", CURRENT_SCHEMA_VERSION)
                .unwrap()
                .unwrap()["count"],
            2
        );
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn section_format_is_rejected_without_importing_or_modifying_it() {
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
            store
                .load_section::<Value>("legacy", CURRENT_SCHEMA_VERSION)
                .unwrap_err()
                .code,
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
        let v3_schema_exists: bool = connection
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='state_nodes')",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(!v3_schema_exists);
        let version: u32 = connection
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .unwrap();
        assert_eq!(version, 0);
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn unchanged_tree_nodes_are_not_updated() {
        let path = std::env::temp_dir().join(format!("loom-persistence-{}.db", Uuid::new_v4()));
        let store = FilePersistence::open(&path).unwrap();
        let value = serde_json::json!({"session": {"name": "alpha", "sequence": 1}});
        store
            .save_sections(CURRENT_SCHEMA_VERSION, &[("sessions", value.clone())])
            .unwrap();
        let connection = Connection::open(&path).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE node_updates(count INTEGER NOT NULL);
                 INSERT INTO node_updates VALUES (0);
                 CREATE TRIGGER count_node_updates AFTER UPDATE ON state_nodes
                 BEGIN UPDATE node_updates SET count = count + 1; END;",
            )
            .unwrap();
        drop(connection);

        store
            .save_sections(CURRENT_SCHEMA_VERSION, &[("sessions", value)])
            .unwrap();
        let connection = Connection::open(&path).unwrap();
        let updates: u32 = connection
            .query_row("SELECT count FROM node_updates", [], |row| row.get(0))
            .unwrap();
        assert_eq!(updates, 0);
        drop(connection);

        store
            .save_sections(
                CURRENT_SCHEMA_VERSION,
                &[(
                    "sessions",
                    serde_json::json!({"session": {"name": "beta", "sequence": 1}}),
                )],
            )
            .unwrap();
        let connection = Connection::open(&path).unwrap();
        let updates: u32 = connection
            .query_row("SELECT count FROM node_updates", [], |row| row.get(0))
            .unwrap();
        assert_eq!(updates, 1);
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn large_text_is_deduplicated_and_compressed() {
        let path = std::env::temp_dir().join(format!("loom-persistence-{}.db", Uuid::new_v4()));
        let store = FilePersistence::open(&path).unwrap();
        let text = "checkpoint content that compresses well ".repeat(2_000);
        store
            .save_sections(
                CURRENT_SCHEMA_VERSION,
                &[
                    ("one", serde_json::json!({"content": text})),
                    ("two", serde_json::json!({"content": text})),
                ],
            )
            .unwrap();

        let connection = Connection::open(&path).unwrap();
        let (blob_count, codec, payload_size, raw_size): (i64, i64, i64, i64) = connection
            .query_row(
                "SELECT COUNT(*), MAX(codec), MAX(length(payload)), MAX(raw_size) FROM content_blobs",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .unwrap();
        assert_eq!(blob_count, 1);
        assert_eq!(codec, 1);
        assert!(payload_size < raw_size);
        assert_eq!(
            store
                .load_section::<Value>("two", CURRENT_SCHEMA_VERSION)
                .unwrap()
                .unwrap()["content"],
            text
        );
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

        persistence
            .save_state_with_sessions(CURRENT_SCHEMA_VERSION, &state, &[])
            .unwrap();
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
        persistence
            .save_state_with_sessions(CURRENT_SCHEMA_VERSION, &state, &[])
            .unwrap();
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
            .save_state_with_catalogs_entities_and_feed(
                CURRENT_SCHEMA_VERSION,
                &SessionManager::default().export_state(),
                &state,
                &[],
                None,
                &[],
            )
            .unwrap();
        assert_eq!(persistence.load_workspaces().unwrap().unwrap(), state);

        let mut changed_manager = WorkspaceManager::default();
        let changed = changed_manager.create("Changed workspace").unwrap();
        assert!(
            persistence
                .save_state_with_catalogs_entities_and_feed(
                    CURRENT_SCHEMA_VERSION,
                    &SessionManager::default().export_state(),
                    &changed_manager.export_state(),
                    &[],
                    None,
                    &[("", serde_json::json!({"invalid": true}))],
                )
                .is_err()
        );
        assert_eq!(persistence.load_workspaces().unwrap().unwrap(), state);
        assert!(!state.workspaces.contains_key(&changed.id));

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
            .save_state_with_catalogs_entities_and_feed(
                CURRENT_SCHEMA_VERSION,
                &SessionManager::default().export_state(),
                &state,
                &[],
                None,
                &[],
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
                evidence: Vec::new(),
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
        let run_summaries = BTreeMap::from([(run_id, run_summary)]);
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
                call: activity_call,
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
        let filesystem_records = [DurableFilesystemRecord {
            session_id: session.id,
            root: "/tmp/loom-session-fs".to_owned(),
            control: WorkspaceControl::Agent,
            checkpoints: vec![checkpoint.clone()],
            payload: serde_json::json!({
                "filesystem": {
                    "session_id": session.id,
                    "root": "/tmp/loom-session-fs",
                    "control": "agent",
                    "checkpoints": []
                },
                "details": "checkpoint state ".repeat(500)
            }),
        }];
        persistence
            .save_state(DurableStateWrite {
                schema_version: CURRENT_SCHEMA_VERSION,
                sessions: &sessions.export_state(),
                workspaces: Some(&workspaces.export_state()),
                settings: Some(&settings),
                workspace_configs: Some(&configs),
                providers: Some(&provider_state),
                usage: Some(&usage),
                idempotency: Some(&idempotency),
                run_summaries: Some(&run_summaries),
                run_messages: Some(&run_messages),
                run_activities: Some(&run_activities),
                filesystem_records: Some(&filesystem_records),
                records: &[],
                feed: None,
                sections: &[],
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
        let mut completed_activity = activity.clone();
        completed_activity.status = AgentActivityStatus::Completed;
        completed_activity.completed_at = Some(Timestamp::from_unix_millis(1600));
        completed_activity.elapsed_ms = Some(100);
        let completed_activities = BTreeMap::from([(run_id, vec![completed_activity])]);
        let invalid_sections = [("", serde_json::json!({"invalid": true}))];
        assert!(
            persistence
                .save_state(DurableStateWrite {
                    schema_version: CURRENT_SCHEMA_VERSION,
                    sessions: &sessions.export_state(),
                    workspaces: None,
                    settings: None,
                    workspace_configs: None,
                    providers: None,
                    usage: None,
                    idempotency: None,
                    run_summaries: None,
                    run_messages: None,
                    run_activities: Some(&completed_activities),
                    filesystem_records: None,
                    records: &[],
                    feed: None,
                    sections: &invalid_sections,
                })
                .is_err()
        );
        assert_eq!(
            persistence.load_run_activities(run_id).unwrap(),
            vec![activity]
        );
        let loaded_idempotency = persistence.load_idempotency_records().unwrap();
        assert_eq!(loaded_idempotency.len(), 1);
        let loaded_record = &loaded_idempotency[&request_id];
        assert_eq!(loaded_record.created_at, Timestamp::from_unix_millis(1234));
        assert_eq!(
            loaded_record.request,
            serde_json::json!({"method": "list_sessions"})
        );
        assert_eq!(loaded_record.response, serde_json::json!({"sessions": []}));
        assert_eq!(persistence.load_run_summaries().unwrap(), run_summaries);
        assert_eq!(
            persistence.load_run_messages(run_id).unwrap(),
            run_messages[&run_id]
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
        assert_eq!(loaded_filesystem.checkpoints, vec![checkpoint.clone()]);

        assert!(
            persistence
                .save_state(DurableStateWrite {
                    schema_version: CURRENT_SCHEMA_VERSION,
                    sessions: &sessions.export_state(),
                    workspaces: Some(&workspaces.export_state()),
                    settings: Some(&DurableSessionSettings::default()),
                    workspace_configs: Some(&BTreeMap::new()),
                    providers: Some(&DurableProviderState::default()),
                    usage: Some(&UsageLedger::default()),
                    idempotency: Some(&BTreeMap::new()),
                    run_summaries: Some(&BTreeMap::new()),
                    run_messages: None,
                    run_activities: None,
                    filesystem_records: Some(&[DurableFilesystemRecord {
                        checkpoints: Vec::new(),
                        ..filesystem_records[0].clone()
                    }]),
                    records: &[],
                    feed: None,
                    sections: &[("", serde_json::json!({"invalid": true}))],
                })
                .is_err()
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
        assert_eq!(retained_filesystem.payload, filesystem_records[0].payload);
        assert_eq!(retained_filesystem.checkpoints, vec![checkpoint.clone()]);

        let connection = Connection::open(&path).unwrap();
        let plan: String = connection
            .query_row(
                "EXPLAIN QUERY PLAN SELECT revision, config FROM workspace_configs WHERE workspace_id=?1",
                [workspace.id.as_uuid().as_bytes().as_slice()],
                |row| row.get(3),
            )
            .unwrap();
        assert!(plan.contains("PRIMARY KEY"), "{plan}");
        let idempotency_plan: String = connection
            .query_row(
                "EXPLAIN QUERY PLAN SELECT request_id FROM idempotency_records
                 WHERE created_at<?1 ORDER BY created_at, request_id LIMIT 32",
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
        assert_eq!(content_count, 3);
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
                schema_version: CURRENT_SCHEMA_VERSION,
                sessions: &sessions.export_state(),
                workspaces: Some(&workspaces.export_state()),
                settings: Some(&settings),
                workspace_configs: Some(&configs),
                providers: Some(&provider_state),
                usage: Some(&usage),
                idempotency: Some(&idempotency),
                run_summaries: Some(&run_summaries),
                run_messages: None,
                run_activities: None,
                filesystem_records: Some(&empty_filesystem_records),
                records: &[],
                feed: None,
                sections: &[],
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
            .query_row("SELECT COUNT(*) FROM content_blobs", [], |row| row.get(0))
            .unwrap();
        assert_eq!(
            content_count, 2,
            "retained transcript and activity content remain reachable"
        );
        drop(connection);
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
        persistence
            .save_state(DurableStateWrite {
                schema_version: CURRENT_SCHEMA_VERSION,
                sessions: &manager.export_state(),
                workspaces: None,
                settings: None,
                workspace_configs: None,
                providers: None,
                usage: None,
                idempotency: None,
                run_summaries: Some(&run_summaries),
                run_messages: Some(&run_messages),
                run_activities: None,
                filesystem_records: None,
                records: &[],
                feed: None,
                sections: &[],
            })
            .unwrap();

        persistence
            .append_run_message_fragment(run_id, session.id, 1, 0, 4, b"hello ")
            .unwrap();
        persistence
            .append_run_message_fragment(run_id, session.id, 1, 0, 4, b"hello ")
            .unwrap();
        persistence
            .append_run_message_fragment(run_id, session.id, 1, 1, 10, b"world")
            .unwrap();
        let newest_page = persistence
            .load_run_message_page(run_id, Some(2), 1)
            .unwrap();
        assert_eq!(newest_page.len(), 1);
        assert_eq!(newest_page[0].ordinal, 1);
        assert_eq!(newest_page[0].content_bytes, 15);
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
                schema_version: CURRENT_SCHEMA_VERSION,
                sessions: &manager.export_state(),
                workspaces: None,
                settings: None,
                workspace_configs: None,
                providers: None,
                usage: None,
                idempotency: None,
                run_summaries: Some(&run_summaries),
                run_messages: Some(&run_messages),
                run_activities: None,
                filesystem_records: None,
                records: &[],
                feed: None,
                sections: &[],
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
                schema_version: CURRENT_SCHEMA_VERSION,
                sessions: &manager.export_state(),
                workspaces: None,
                settings: None,
                workspace_configs: None,
                providers: None,
                usage: None,
                idempotency: None,
                run_summaries: Some(&run_summaries),
                run_messages: Some(&mismatched_messages),
                run_activities: None,
                filesystem_records: None,
                records: &[],
                feed: None,
                sections: &[],
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
        assert!(
            persistence
                .save_state(DurableStateWrite {
                    schema_version: CURRENT_SCHEMA_VERSION,
                    sessions: &manager.export_state(),
                    workspaces: None,
                    settings: None,
                    workspace_configs: None,
                    providers: None,
                    usage: None,
                    idempotency: None,
                    run_summaries: Some(&run_summaries),
                    run_messages: Some(&assembled_messages),
                    run_activities: None,
                    filesystem_records: None,
                    records: &[],
                    feed: None,
                    sections: &[("", serde_json::json!({"invalid": true}))],
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
                schema_version: CURRENT_SCHEMA_VERSION,
                sessions: &manager.export_state(),
                workspaces: None,
                settings: None,
                workspace_configs: None,
                providers: None,
                usage: None,
                idempotency: None,
                run_summaries: Some(&run_summaries),
                run_messages: Some(&assembled_messages),
                run_activities: None,
                filesystem_records: None,
                records: &[],
                feed: None,
                sections: &[],
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
    fn typed_session_rows_roll_back_with_the_rest_of_the_state_write() {
        let path = std::env::temp_dir().join(format!("loom-persistence-{}.db", Uuid::new_v4()));
        let persistence = FilePersistence::open(&path).unwrap();
        let mut manager = SessionManager::default();
        manager
            .create_in_workspace(WorkspaceId::new(), "Atomic session")
            .unwrap();
        let state = manager.export_state();

        assert!(
            persistence
                .save_state_with_sessions(
                    CURRENT_SCHEMA_VERSION,
                    &state,
                    &[("", serde_json::json!({"invalid": true}))],
                )
                .is_err()
        );
        assert!(persistence.load_sessions().unwrap().is_none());
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn run_sections_can_be_listed_and_loaded_by_subtree() {
        let path = std::env::temp_dir().join(format!("loom-persistence-{}.db", Uuid::new_v4()));
        let store = FilePersistence::open(&path).unwrap();
        let first = format!("run:{}", Uuid::new_v4());
        let second = format!("run:{}", Uuid::new_v4());
        store
            .save_sections(
                CURRENT_SCHEMA_VERSION,
                &[
                    (
                        &first,
                        serde_json::json!({
                            "run": {"state": "completed"},
                            "messages": ["small summary", "large transcript".repeat(1_000)]
                        }),
                    ),
                    (
                        &second,
                        serde_json::json!({"run": {"state": "failed"}, "messages": []}),
                    ),
                ],
            )
            .unwrap();
        assert_eq!(store.list_sections_with_prefix("run:").unwrap().len(), 2);
        assert_eq!(
            store
                .load_section_path::<Value>(&first, "/krun", CURRENT_SCHEMA_VERSION)
                .unwrap()
                .unwrap(),
            serde_json::json!({"state": "completed"})
        );
        assert!(
            store
                .load_section_path::<Value>(&first, "/kmissing", CURRENT_SCHEMA_VERSION)
                .unwrap()
                .is_none()
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
        };
        let save =
            |summary: &BTreeMap<RunId, DurableRunSummary>, feed: &DurableFeedState| -> Result<()> {
                persistence.save_state(DurableStateWrite {
                    schema_version: CURRENT_SCHEMA_VERSION,
                    sessions: &manager.export_state(),
                    workspaces: None,
                    settings: None,
                    workspace_configs: None,
                    providers: None,
                    usage: None,
                    idempotency: None,
                    run_summaries: Some(summary),
                    run_messages: None,
                    run_activities: None,
                    filesystem_records: None,
                    records: &[],
                    feed: Some(feed),
                    sections: &[],
                })
            };
        save(&run_summaries, &feed).unwrap();
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
        assert_eq!(
            save(&resolved_summaries, &feed).unwrap_err().code,
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
        let persisted_feed = persistence.load_feed_state().unwrap().unwrap();
        assert_eq!(persisted_feed.next_sequence, EventSequence::new(1));
        assert_eq!(persisted_feed.events, vec![input_event]);

        feed.next_sequence = EventSequence::new(2);
        save(&resolved_summaries, &feed).unwrap();
        assert_eq!(
            persistence.load_run_attempts(run_id).unwrap(),
            vec![AgentRunAttemptRecord {
                state: AgentRunState::Executing,
                ..attempt.clone()
            }]
        );
        assert_eq!(
            persistence.load_run_execution_state(run_id).unwrap(),
            Some(execution.clone())
        );
        assert_eq!(
            persistence.load_run_interactions(run_id).unwrap(),
            vec![interaction.clone()]
        );
        let persisted_feed = persistence.load_feed_state().unwrap().unwrap();
        assert_eq!(persisted_feed.next_sequence, EventSequence::new(2));
        assert_eq!(persisted_feed.events.len(), 2);

        summary.snapshot.state = AgentRunState::Evaluating;
        summary.snapshot.updated_at = Timestamp::from_unix_millis(3_000);
        summary.attempts = Some(vec![AgentRunAttemptRecord {
            state: AgentRunState::Evaluating,
            ..attempt.clone()
        }]);
        execution.state = AgentRunState::Evaluating;
        summary.execution_state = Some(execution.clone());
        let evaluating_summaries = BTreeMap::from([(run_id, summary)]);
        save(&evaluating_summaries, &feed).unwrap();
        assert_eq!(
            persistence.load_run_execution_state(run_id).unwrap(),
            Some(execution)
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
        let sessions = manager.export_state();
        let feed = DurableFeedState {
            next_sequence: EventSequence::new(3),
            retention_limit: 2,
            events: (1..=3)
                .map(|sequence| ServerEventEnvelope {
                    protocol_version: loom_protocol::CURRENT_PROTOCOL_VERSION,
                    sequence: EventSequence::new(sequence),
                    session_id,
                    event: loom_protocol::ServerEvent::AgentSessionCreated {
                        snapshot: snapshot.clone(),
                    },
                })
                .collect(),
        };
        store
            .save_state_with_sessions_entities_and_feed(
                CURRENT_SCHEMA_VERSION,
                &sessions,
                &[],
                Some(&feed),
                &[],
            )
            .unwrap();
        let loaded = store.load_feed_state().unwrap().unwrap();
        assert_eq!(loaded.next_sequence, EventSequence::new(3));
        assert_eq!(loaded.retention_limit, 2);
        assert_eq!(
            loaded
                .events
                .iter()
                .map(|event| event.sequence.value())
                .collect::<Vec<_>>(),
            vec![2, 3]
        );

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
}
