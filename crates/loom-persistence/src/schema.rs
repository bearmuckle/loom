pub(crate) const DATABASE_SCHEMA: &str = "
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
CREATE TABLE IF NOT EXISTS sessions_hierarchy (
    project_id BLOB NOT NULL REFERENCES sessions(id) ON DELETE CASCADE
        CHECK(length(project_id) = 16),
    session_id BLOB PRIMARY KEY NOT NULL REFERENCES sessions(id) ON DELETE CASCADE
        CHECK(length(session_id) = 16),
    parent_session_id BLOB REFERENCES sessions(id) ON DELETE CASCADE
        CHECK(parent_session_id IS NULL OR length(parent_session_id) = 16),
    depth INTEGER NOT NULL CHECK(depth BETWEEN 1 AND 3),
    CHECK(parent_session_id IS NULL OR parent_session_id != session_id),
    CHECK((parent_session_id IS NULL AND depth = 1)
       OR (parent_session_id IS NOT NULL AND depth > 1)),
    CHECK(parent_session_id IS NOT NULL OR project_id = session_id),
    UNIQUE(project_id, session_id),
    FOREIGN KEY(project_id, parent_session_id)
        REFERENCES sessions_hierarchy(project_id, session_id) ON DELETE CASCADE
) WITHOUT ROWID, STRICT;
CREATE INDEX IF NOT EXISTS sessions_hierarchy_by_project
    ON sessions_hierarchy(project_id, depth, session_id);
CREATE INDEX IF NOT EXISTS sessions_hierarchy_by_parent
    ON sessions_hierarchy(parent_session_id, session_id)
    WHERE parent_session_id IS NOT NULL;
CREATE TRIGGER IF NOT EXISTS sessions_hierarchy_parent_depth_insert
BEFORE INSERT ON sessions_hierarchy
WHEN NEW.parent_session_id IS NOT NULL AND NOT EXISTS (
    SELECT 1 FROM sessions_hierarchy AS parent
    WHERE parent.project_id=NEW.project_id
      AND parent.session_id=NEW.parent_session_id
      AND parent.depth + 1=NEW.depth
)
BEGIN
    SELECT RAISE(ABORT, 'project parent must belong to same project at previous depth');
END;
CREATE TRIGGER IF NOT EXISTS sessions_hierarchy_parent_depth_update
BEFORE UPDATE OF project_id, parent_session_id, depth ON sessions_hierarchy
WHEN NEW.parent_session_id IS NOT NULL AND NOT EXISTS (
    SELECT 1 FROM sessions_hierarchy AS parent
    WHERE parent.project_id=NEW.project_id
      AND parent.session_id=NEW.parent_session_id
      AND parent.depth + 1=NEW.depth
)
BEGIN
    SELECT RAISE(ABORT, 'project parent must belong to same project at previous depth');
END;
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
    context_inspection TEXT,
    -- One versioned JSON payload holds every per-run project-agent grant.
    -- Adding a grant is a serde-default field, not a schema migration.
    project_grants TEXT NOT NULL DEFAULT '{}'
        CHECK(length(CAST(project_grants AS BLOB)) <= 4096)
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
    last_project_message_sequence INTEGER NOT NULL DEFAULT 0 CHECK(last_project_message_sequence >= 0),
    pending_tool_execution TEXT CHECK(
        pending_tool_execution IS NULL OR length(pending_tool_execution) <= 1048576
    ),
    pending_approval TEXT CHECK(
        pending_approval IS NULL OR length(pending_approval) <= 1048576
    ),
    pending_input TEXT CHECK(pending_input IS NULL OR length(pending_input) <= 65536),
    last_failed_call TEXT CHECK(last_failed_call IS NULL OR length(last_failed_call) <= 1048576),
    pending_project_join TEXT CHECK(
        pending_project_join IS NULL OR length(pending_project_join) <= 1048576
    ),
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
    timeline_ordinal INTEGER NOT NULL DEFAULT 0 CHECK(timeline_ordinal >= 0),
    role TEXT NOT NULL CHECK(role IN ('system', 'user', 'assistant', 'tool')),
    content_hash BLOB REFERENCES content_objects(hash) ON DELETE RESTRICT
        CHECK(content_hash IS NULL OR length(content_hash) = 32),
    name TEXT,
    tool_call_id BLOB CHECK(tool_call_id IS NULL OR length(tool_call_id) = 16),
    -- Message tool calls and streamed fragment descriptors are versioned JSON
    -- payloads on the message row, so a transcript message is one row.
    tool_calls TEXT NOT NULL DEFAULT '[]'
        CHECK(length(CAST(tool_calls AS BLOB)) <= 16777216),
    fragments TEXT NOT NULL DEFAULT '[]'
        CHECK(length(CAST(fragments AS BLOB)) <= 1048576),
    PRIMARY KEY(run_id, ordinal),
    FOREIGN KEY(run_id, session_id)
        REFERENCES run_summaries(run_id, session_id) ON DELETE CASCADE
) WITHOUT ROWID, STRICT;
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
    timeline_ordinal INTEGER NOT NULL DEFAULT 0 CHECK(timeline_ordinal >= 0),
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
    -- Tool execution attempts are a versioned JSON payload on the logical call.
    attempts TEXT NOT NULL DEFAULT '[]'
        CHECK(length(CAST(attempts AS BLOB)) <= 16777216),
    PRIMARY KEY(run_id, tool_call_id),
    FOREIGN KEY(run_id, session_id)
        REFERENCES run_summaries(run_id, session_id) ON DELETE CASCADE
) WITHOUT ROWID, STRICT;
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

pub(crate) const PROJECT_TASK_SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS delegated_tasks (
    task_id BLOB PRIMARY KEY NOT NULL CHECK(length(task_id) = 16),
    request_id BLOB NOT NULL UNIQUE CHECK(length(request_id) = 16),
    request_fingerprint BLOB NOT NULL CHECK(length(request_fingerprint) = 32),
    project_id BLOB NOT NULL CHECK(length(project_id) = 16),
    requester_session_id BLOB NOT NULL CHECK(length(requester_session_id) = 16),
    target_session_id BLOB NOT NULL UNIQUE CHECK(length(target_session_id) = 16),
    child_name TEXT NOT NULL CHECK(length(trim(child_name)) > 0),
    intent TEXT NOT NULL CHECK(length(trim(intent)) > 0),
    model_id TEXT NOT NULL CHECK(length(trim(model_id)) > 0),
    code_change INTEGER NOT NULL CHECK(code_change IN (0, 1)),
    -- One versioned JSON payload holds the child's independent grants.
    permissions TEXT NOT NULL DEFAULT '{}'
        CHECK(length(CAST(permissions AS BLOB)) <= 4096),
    status TEXT NOT NULL CHECK(status IN ('queued', 'running', 'blocked', 'completed', 'failed', 'cancelled')),
    created_at INTEGER NOT NULL CHECK(created_at >= 0),
    updated_at INTEGER NOT NULL CHECK(updated_at >= created_at),
    FOREIGN KEY(project_id, requester_session_id)
        REFERENCES sessions_hierarchy(project_id, session_id) ON DELETE CASCADE,
    FOREIGN KEY(project_id, target_session_id)
        REFERENCES sessions_hierarchy(project_id, session_id) ON DELETE CASCADE
) WITHOUT ROWID, STRICT;
CREATE INDEX IF NOT EXISTS delegated_tasks_by_requester
    ON delegated_tasks(project_id, requester_session_id, created_at, task_id);
CREATE INDEX IF NOT EXISTS delegated_tasks_by_status
    ON delegated_tasks(project_id, status, updated_at, task_id);
CREATE TABLE IF NOT EXISTS delegated_task_context_references (
    task_id BLOB NOT NULL REFERENCES delegated_tasks(task_id) ON DELETE CASCADE
        CHECK(length(task_id) = 16),
    ordinal INTEGER NOT NULL CHECK(ordinal >= 0),
    label TEXT NOT NULL,
    uri TEXT NOT NULL,
    PRIMARY KEY(task_id, ordinal)
) WITHOUT ROWID, STRICT;
CREATE TABLE IF NOT EXISTS delegated_task_dependencies (
    task_id BLOB NOT NULL REFERENCES delegated_tasks(task_id) ON DELETE CASCADE
        CHECK(length(task_id) = 16),
    ordinal INTEGER NOT NULL CHECK(ordinal >= 0),
    dependency_task_id BLOB NOT NULL REFERENCES delegated_tasks(task_id) ON DELETE RESTRICT
        CHECK(length(dependency_task_id) = 16),
    CHECK(task_id != dependency_task_id),
    PRIMARY KEY(task_id, ordinal),
    UNIQUE(task_id, dependency_task_id)
) WITHOUT ROWID, STRICT;
CREATE INDEX IF NOT EXISTS delegated_task_dependencies_by_dependency
    ON delegated_task_dependencies(dependency_task_id, task_id);
CREATE TABLE IF NOT EXISTS project_message_sequences (
    project_id BLOB PRIMARY KEY NOT NULL REFERENCES sessions(id) ON DELETE CASCADE
        CHECK(length(project_id) = 16),
    next_sequence INTEGER NOT NULL CHECK(next_sequence >= 1)
) WITHOUT ROWID, STRICT;
CREATE TABLE IF NOT EXISTS project_agent_messages (
    message_id BLOB PRIMARY KEY NOT NULL CHECK(length(message_id) = 16),
    request_id BLOB NOT NULL UNIQUE CHECK(length(request_id) = 16),
    project_id BLOB NOT NULL CHECK(length(project_id) = 16),
    project_sequence INTEGER NOT NULL CHECK(project_sequence >= 1),
    task_id BLOB CHECK(task_id IS NULL OR length(task_id) = 16),
    sender_session_id BLOB NOT NULL CHECK(length(sender_session_id) = 16),
    target_session_id BLOB NOT NULL CHECK(length(target_session_id) = 16),
    kind TEXT NOT NULL CHECK(kind IN ('progress', 'result', 'question', 'blocker', 'direction', 'answer')),
    accepted_at INTEGER NOT NULL CHECK(accepted_at >= 0),
    body TEXT NOT NULL,
    UNIQUE(project_id, project_sequence),
    FOREIGN KEY(project_id, sender_session_id)
        REFERENCES sessions_hierarchy(project_id, session_id) ON DELETE CASCADE,
    FOREIGN KEY(project_id, target_session_id)
        REFERENCES sessions_hierarchy(project_id, session_id) ON DELETE CASCADE,
    FOREIGN KEY(task_id)
        REFERENCES delegated_tasks(task_id) ON DELETE RESTRICT
) WITHOUT ROWID, STRICT;
CREATE INDEX IF NOT EXISTS project_agent_messages_by_target
    ON project_agent_messages(project_id, target_session_id, project_sequence);
CREATE INDEX IF NOT EXISTS project_agent_messages_by_sender
    ON project_agent_messages(project_id, sender_session_id, project_sequence);
";

pub(crate) const PROJECT_WORKTREE_SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS project_worktrees (
    task_id BLOB PRIMARY KEY NOT NULL CHECK(length(task_id) = 16),
    project_id BLOB NOT NULL CHECK(length(project_id) = 16),
    parent_session_id BLOB NOT NULL CHECK(length(parent_session_id) = 16),
    child_session_id BLOB NOT NULL CHECK(length(child_session_id) = 16),
    parent_repository_id BLOB NOT NULL CHECK(length(parent_repository_id) = 16),
    child_repository_id BLOB NOT NULL CHECK(length(child_repository_id) = 16),
    relative_path TEXT NOT NULL CHECK(length(relative_path) > 0 AND length(relative_path) <= 4096
        AND substr(relative_path, 1, 1) != '/' AND instr(relative_path, char(0)) = 0
        AND instr(relative_path, char(92)) = 0 AND relative_path != '..'
        AND relative_path NOT LIKE '../%' AND relative_path NOT LIKE '%/../%'
        AND relative_path NOT LIKE '%/..'),
    worktree_name TEXT NOT NULL CHECK(length(trim(worktree_name)) > 0 AND length(worktree_name) <= 256),
    branch_name TEXT NOT NULL CHECK(length(trim(branch_name)) > 0 AND length(branch_name) <= 256),
    base_revision TEXT NOT NULL CHECK(length(trim(base_revision)) > 0 AND length(base_revision) <= 256),
    result_revision TEXT CHECK(result_revision IS NULL OR length(result_revision) <= 256),
    integrated_revision TEXT CHECK(integrated_revision IS NULL OR length(integrated_revision) <= 256),
    status TEXT NOT NULL CHECK(status IN (
        'creating', 'ready', 'stale', 'conflict', 'integrating', 'integrated', 'recovery_required',
        'cleanup_pending', 'retained', 'removed'
    )),
    error TEXT CHECK(error IS NULL OR length(error) <= 8192),
    cleanup_disposition TEXT CHECK(cleanup_disposition IS NULL OR cleanup_disposition IN (
        'retain', 'remove_clean', 'discard_changes'
    )),
    created_at INTEGER NOT NULL CHECK(created_at >= 0),
    updated_at INTEGER NOT NULL CHECK(updated_at >= created_at),
    UNIQUE(project_id, task_id),
    FOREIGN KEY(task_id) REFERENCES delegated_tasks(task_id) ON DELETE CASCADE,
    FOREIGN KEY(project_id, parent_session_id)
        REFERENCES sessions_hierarchy(project_id, session_id) ON DELETE CASCADE,
    FOREIGN KEY(project_id, child_session_id)
        REFERENCES sessions_hierarchy(project_id, session_id) ON DELETE CASCADE
) WITHOUT ROWID, STRICT;
CREATE INDEX IF NOT EXISTS project_worktrees_by_project_status
    ON project_worktrees(project_id, status, updated_at, task_id);
CREATE TABLE IF NOT EXISTS project_worktree_conflict_paths (
    project_id BLOB NOT NULL CHECK(length(project_id) = 16),
    task_id BLOB NOT NULL CHECK(length(task_id) = 16),
    ordinal INTEGER NOT NULL CHECK(ordinal >= 0),
    path TEXT NOT NULL CHECK(length(path) > 0 AND length(path) <= 4096),
    PRIMARY KEY(project_id, task_id, ordinal),
    UNIQUE(project_id, task_id, path),
    FOREIGN KEY(project_id, task_id)
        REFERENCES project_worktrees(project_id, task_id) ON DELETE CASCADE
) WITHOUT ROWID, STRICT;
CREATE INDEX IF NOT EXISTS project_worktree_conflicts_by_path
    ON project_worktree_conflict_paths(project_id, path, task_id);
";

pub(crate) const PROJECT_MANAGER_WAIT_SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS project_manager_waits (
    wait_id BLOB PRIMARY KEY NOT NULL CHECK(length(wait_id) = 16),
    run_id BLOB NOT NULL CHECK(length(run_id) = 16),
    attempt_id BLOB NOT NULL CHECK(length(attempt_id) = 16),
    tool_call_id BLOB NOT NULL CHECK(length(tool_call_id) = 16),
    manager_session_id BLOB NOT NULL CHECK(length(manager_session_id) = 16),
    status TEXT NOT NULL CHECK(status IN ('waiting', 'ready', 'resuming', 'consumed', 'abandoned')),
    result_summary TEXT CHECK(result_summary IS NULL OR length(CAST(result_summary AS BLOB)) <= 65536),
    created_at INTEGER NOT NULL CHECK(created_at >= 0),
    updated_at INTEGER NOT NULL CHECK(updated_at >= created_at),
    UNIQUE(run_id, attempt_id, tool_call_id),
    FOREIGN KEY(run_id, manager_session_id)
        REFERENCES run_summaries(run_id, session_id) ON DELETE CASCADE,
    FOREIGN KEY(run_id, attempt_id)
        REFERENCES run_attempts(run_id, attempt_id) ON DELETE CASCADE,
    FOREIGN KEY(run_id, tool_call_id)
        REFERENCES run_tool_calls(run_id, tool_call_id) ON DELETE CASCADE
) WITHOUT ROWID, STRICT;
CREATE INDEX IF NOT EXISTS project_manager_waits_by_status
    ON project_manager_waits(status, updated_at, wait_id);
CREATE INDEX IF NOT EXISTS project_manager_waits_by_manager
    ON project_manager_waits(manager_session_id, status, updated_at, wait_id);
CREATE TABLE IF NOT EXISTS project_manager_wait_children (
    wait_id BLOB NOT NULL REFERENCES project_manager_waits(wait_id) ON DELETE CASCADE
        CHECK(length(wait_id) = 16),
    ordinal INTEGER NOT NULL CHECK(ordinal >= 0),
    child_task_id BLOB NOT NULL REFERENCES delegated_tasks(task_id) ON DELETE CASCADE
        CHECK(length(child_task_id) = 16),
    PRIMARY KEY(wait_id, ordinal),
    UNIQUE(wait_id, child_task_id)
) WITHOUT ROWID, STRICT;
CREATE INDEX IF NOT EXISTS project_manager_wait_children_by_task
    ON project_manager_wait_children(child_task_id, wait_id);
";

pub(crate) const PROJECT_CANCELLATION_CASCADE_SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS project_cancellation_cascades (
    project_id BLOB PRIMARY KEY NOT NULL CHECK(length(project_id) = 16),
    root_task_id BLOB NOT NULL CHECK(length(root_task_id) = 16),
    manager_session_id BLOB NOT NULL CHECK(length(manager_session_id) = 16),
    created_at INTEGER NOT NULL CHECK(created_at >= 0),
    FOREIGN KEY(project_id) REFERENCES sessions(id) ON DELETE CASCADE,
    FOREIGN KEY(root_task_id) REFERENCES delegated_tasks(task_id) ON DELETE CASCADE
) WITHOUT ROWID, STRICT;
CREATE TABLE IF NOT EXISTS project_cancellation_cascade_members (
    project_id BLOB NOT NULL CHECK(length(project_id) = 16),
    ordinal INTEGER NOT NULL CHECK(ordinal >= 0),
    task_id BLOB NOT NULL CHECK(length(task_id) = 16),
    target_session_id BLOB NOT NULL CHECK(length(target_session_id) = 16),
    PRIMARY KEY(project_id, ordinal),
    UNIQUE(project_id, task_id),
    UNIQUE(project_id, target_session_id),
    FOREIGN KEY(project_id) REFERENCES project_cancellation_cascades(project_id) ON DELETE CASCADE,
    FOREIGN KEY(task_id) REFERENCES delegated_tasks(task_id) ON DELETE CASCADE
) WITHOUT ROWID, STRICT;
";
