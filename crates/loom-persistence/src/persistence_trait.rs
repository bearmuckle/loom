use std::sync::Arc;

use super::*;

pub trait CatalogRepository: Send + Sync {
    fn load_idempotency_records(&self) -> Result<BTreeMap<RequestId, DurableIdempotencyRecord>>;
    fn load_provider_configs(&self) -> Result<Vec<ProviderConfig>>;
    fn load_provider_health(&self) -> Result<BTreeMap<ProviderId, ProviderHealth>>;
    fn load_provider_usage(&self) -> Result<UsageLedger>;
    fn load_workspace_configs(&self) -> Result<BTreeMap<WorkspaceId, WorkspaceConfig>>;
    fn load_workspaces(&self) -> Result<Option<WorkspaceManagerState>>;
    fn path(&self) -> &Path;
    fn prune_expired_idempotency_records(&self, now: Timestamp) -> Result<usize>;
    fn release_exclusive_writer(&self) -> Result<()>;
    fn save_state(&self, write: DurableStateWrite<'_>) -> Result<()>;
}

impl CatalogRepository for FilePersistence {
    fn load_idempotency_records(&self) -> Result<BTreeMap<RequestId, DurableIdempotencyRecord>> {
        FilePersistence::load_idempotency_records(self)
    }
    fn load_provider_configs(&self) -> Result<Vec<ProviderConfig>> {
        FilePersistence::load_provider_configs(self)
    }
    fn load_provider_health(&self) -> Result<BTreeMap<ProviderId, ProviderHealth>> {
        FilePersistence::load_provider_health(self)
    }
    fn load_provider_usage(&self) -> Result<UsageLedger> {
        FilePersistence::load_provider_usage(self)
    }
    fn load_workspace_configs(&self) -> Result<BTreeMap<WorkspaceId, WorkspaceConfig>> {
        FilePersistence::load_workspace_configs(self)
    }
    fn load_workspaces(&self) -> Result<Option<WorkspaceManagerState>> {
        FilePersistence::load_workspaces(self)
    }
    fn path(&self) -> &Path {
        FilePersistence::path(self)
    }
    fn prune_expired_idempotency_records(&self, now: Timestamp) -> Result<usize> {
        FilePersistence::prune_expired_idempotency_records(self, now)
    }
    fn release_exclusive_writer(&self) -> Result<()> {
        FilePersistence::release_exclusive_writer(self)
    }
    fn save_state(&self, write: DurableStateWrite<'_>) -> Result<()> {
        FilePersistence::save_state(self, write)
    }
}

impl<T: CatalogRepository + ?Sized> CatalogRepository for Arc<T> {
    fn load_idempotency_records(&self) -> Result<BTreeMap<RequestId, DurableIdempotencyRecord>> {
        (**self).load_idempotency_records()
    }
    fn load_provider_configs(&self) -> Result<Vec<ProviderConfig>> {
        (**self).load_provider_configs()
    }
    fn load_provider_health(&self) -> Result<BTreeMap<ProviderId, ProviderHealth>> {
        (**self).load_provider_health()
    }
    fn load_provider_usage(&self) -> Result<UsageLedger> {
        (**self).load_provider_usage()
    }
    fn load_workspace_configs(&self) -> Result<BTreeMap<WorkspaceId, WorkspaceConfig>> {
        (**self).load_workspace_configs()
    }
    fn load_workspaces(&self) -> Result<Option<WorkspaceManagerState>> {
        (**self).load_workspaces()
    }
    fn path(&self) -> &Path {
        (**self).path()
    }
    fn prune_expired_idempotency_records(&self, now: Timestamp) -> Result<usize> {
        (**self).prune_expired_idempotency_records(now)
    }
    fn release_exclusive_writer(&self) -> Result<()> {
        (**self).release_exclusive_writer()
    }
    fn save_state(&self, write: DurableStateWrite<'_>) -> Result<()> {
        (**self).save_state(write)
    }
}

pub trait SessionRepository: Send + Sync {
    fn load_latest_run_summary_for_session(
        &self,
        session_id: AgentSessionId,
    ) -> Result<Option<DurableRunSummary>>;
    fn load_sessions(&self) -> Result<Option<SessionManagerState>>;
    fn load_session_settings(&self) -> Result<DurableSessionSettings>;
    fn load_session_usage(
        &self,
        session_id: AgentSessionId,
        excluded_runs: &BTreeSet<RunId>,
    ) -> Result<UsageSnapshot>;
}

impl SessionRepository for FilePersistence {
    fn load_latest_run_summary_for_session(
        &self,
        session_id: AgentSessionId,
    ) -> Result<Option<DurableRunSummary>> {
        FilePersistence::load_latest_run_summary_for_session(self, session_id)
    }
    fn load_sessions(&self) -> Result<Option<SessionManagerState>> {
        FilePersistence::load_sessions(self)
    }
    fn load_session_settings(&self) -> Result<DurableSessionSettings> {
        FilePersistence::load_session_settings(self)
    }
    fn load_session_usage(
        &self,
        session_id: AgentSessionId,
        excluded_runs: &BTreeSet<RunId>,
    ) -> Result<UsageSnapshot> {
        FilePersistence::load_session_usage(self, session_id, excluded_runs)
    }
}

impl<T: SessionRepository + ?Sized> SessionRepository for Arc<T> {
    fn load_latest_run_summary_for_session(
        &self,
        session_id: AgentSessionId,
    ) -> Result<Option<DurableRunSummary>> {
        (**self).load_latest_run_summary_for_session(session_id)
    }
    fn load_sessions(&self) -> Result<Option<SessionManagerState>> {
        (**self).load_sessions()
    }
    fn load_session_settings(&self) -> Result<DurableSessionSettings> {
        (**self).load_session_settings()
    }
    fn load_session_usage(
        &self,
        session_id: AgentSessionId,
        excluded_runs: &BTreeSet<RunId>,
    ) -> Result<UsageSnapshot> {
        (**self).load_session_usage(session_id, excluded_runs)
    }
}

pub trait RunRepository: Send + Sync {
    fn append_run_message_fragment(
        &self,
        run_id: RunId,
        session_id: AgentSessionId,
        message_ordinal: u64,
        fragment_ordinal: u64,
        byte_offset: u64,
        content: &[u8],
    ) -> Result<()>;
    fn load_active_run_summaries(&self) -> Result<BTreeMap<RunId, DurableRunSummary>>;
    fn load_run_message_page(
        &self,
        run_id: RunId,
        before_ordinal: Option<u64>,
        limit: usize,
    ) -> Result<Vec<DurableRunMessageHeader>>;
    fn load_run_activities(&self, run_id: RunId) -> Result<Vec<AgentActivityRecord>>;
    fn load_run_attempts(&self, run_id: RunId) -> Result<Vec<AgentRunAttemptRecord>>;
    fn load_run_context_checkpoint(
        &self,
        run_id: RunId,
    ) -> Result<Option<DurableRunContextCheckpoint>>;
    fn load_run_execution_state(&self, run_id: RunId) -> Result<Option<AgentExecutionStateRecord>>;
    fn load_run_interactions(&self, run_id: RunId) -> Result<Vec<AgentInteractionRecord>>;
    fn load_run_message_content_range(
        &self,
        run_id: RunId,
        message_ordinal: u64,
        byte_offset: u64,
        length: usize,
    ) -> Result<Vec<u8>>;
    fn load_run_messages(&self, run_id: RunId) -> Result<Vec<DurableRunMessage>>;
    fn load_run_plan(&self, run_id: RunId) -> Result<AgentPlan>;
    fn load_run_runtime_config(&self, run_id: RunId) -> Result<Option<DurableRunRuntimeConfig>>;
    fn load_run_summary(&self, run_id: RunId) -> Result<Option<DurableRunSummary>>;
    fn next_run_message_fragment_position(
        &self,
        run_id: RunId,
        message_ordinal: u64,
    ) -> Result<(u64, u64)>;
    fn save_recovery_updates(
        &self,
        summaries: &BTreeMap<RunId, DurableRunSummary>,
        feed: &DurableFeedState,
    ) -> Result<()>;
    fn save_run_checkpoint(&self, write: DurableRunCheckpointWrite<'_>) -> Result<()>;
    fn save_run_checkpoint_with_project_manager_wait(
        &self,
        write: DurableRunCheckpointWrite<'_>,
        wait: &ProjectManagerWaitRecord,
    ) -> Result<()>;
}

impl RunRepository for FilePersistence {
    fn append_run_message_fragment(
        &self,
        run_id: RunId,
        session_id: AgentSessionId,
        message_ordinal: u64,
        fragment_ordinal: u64,
        byte_offset: u64,
        content: &[u8],
    ) -> Result<()> {
        FilePersistence::append_run_message_fragment(
            self,
            run_id,
            session_id,
            message_ordinal,
            fragment_ordinal,
            byte_offset,
            content,
        )
    }
    fn load_active_run_summaries(&self) -> Result<BTreeMap<RunId, DurableRunSummary>> {
        FilePersistence::load_active_run_summaries(self)
    }
    fn load_run_message_page(
        &self,
        run_id: RunId,
        before_ordinal: Option<u64>,
        limit: usize,
    ) -> Result<Vec<DurableRunMessageHeader>> {
        FilePersistence::load_run_message_page(self, run_id, before_ordinal, limit)
    }
    fn load_run_activities(&self, run_id: RunId) -> Result<Vec<AgentActivityRecord>> {
        FilePersistence::load_run_activities(self, run_id)
    }
    fn load_run_attempts(&self, run_id: RunId) -> Result<Vec<AgentRunAttemptRecord>> {
        FilePersistence::load_run_attempts(self, run_id)
    }
    fn load_run_context_checkpoint(
        &self,
        run_id: RunId,
    ) -> Result<Option<DurableRunContextCheckpoint>> {
        FilePersistence::load_run_context_checkpoint(self, run_id)
    }
    fn load_run_execution_state(&self, run_id: RunId) -> Result<Option<AgentExecutionStateRecord>> {
        FilePersistence::load_run_execution_state(self, run_id)
    }
    fn load_run_interactions(&self, run_id: RunId) -> Result<Vec<AgentInteractionRecord>> {
        FilePersistence::load_run_interactions(self, run_id)
    }
    fn load_run_message_content_range(
        &self,
        run_id: RunId,
        message_ordinal: u64,
        byte_offset: u64,
        length: usize,
    ) -> Result<Vec<u8>> {
        FilePersistence::load_run_message_content_range(
            self,
            run_id,
            message_ordinal,
            byte_offset,
            length,
        )
    }
    fn load_run_messages(&self, run_id: RunId) -> Result<Vec<DurableRunMessage>> {
        FilePersistence::load_run_messages(self, run_id)
    }
    fn load_run_plan(&self, run_id: RunId) -> Result<AgentPlan> {
        FilePersistence::load_run_plan(self, run_id)
    }
    fn load_run_runtime_config(&self, run_id: RunId) -> Result<Option<DurableRunRuntimeConfig>> {
        FilePersistence::load_run_runtime_config(self, run_id)
    }
    fn load_run_summary(&self, run_id: RunId) -> Result<Option<DurableRunSummary>> {
        FilePersistence::load_run_summary(self, run_id)
    }
    fn next_run_message_fragment_position(
        &self,
        run_id: RunId,
        message_ordinal: u64,
    ) -> Result<(u64, u64)> {
        FilePersistence::next_run_message_fragment_position(self, run_id, message_ordinal)
    }
    fn save_recovery_updates(
        &self,
        summaries: &BTreeMap<RunId, DurableRunSummary>,
        feed: &DurableFeedState,
    ) -> Result<()> {
        FilePersistence::save_recovery_updates(self, summaries, feed)
    }
    fn save_run_checkpoint(&self, write: DurableRunCheckpointWrite<'_>) -> Result<()> {
        FilePersistence::save_run_checkpoint(self, write)
    }
    fn save_run_checkpoint_with_project_manager_wait(
        &self,
        write: DurableRunCheckpointWrite<'_>,
        wait: &ProjectManagerWaitRecord,
    ) -> Result<()> {
        FilePersistence::save_run_checkpoint_with_project_manager_wait(self, write, wait)
    }
}

impl<T: RunRepository + ?Sized> RunRepository for Arc<T> {
    fn append_run_message_fragment(
        &self,
        run_id: RunId,
        session_id: AgentSessionId,
        message_ordinal: u64,
        fragment_ordinal: u64,
        byte_offset: u64,
        content: &[u8],
    ) -> Result<()> {
        (**self).append_run_message_fragment(
            run_id,
            session_id,
            message_ordinal,
            fragment_ordinal,
            byte_offset,
            content,
        )
    }
    fn load_active_run_summaries(&self) -> Result<BTreeMap<RunId, DurableRunSummary>> {
        (**self).load_active_run_summaries()
    }
    fn load_run_message_page(
        &self,
        run_id: RunId,
        before_ordinal: Option<u64>,
        limit: usize,
    ) -> Result<Vec<DurableRunMessageHeader>> {
        (**self).load_run_message_page(run_id, before_ordinal, limit)
    }
    fn load_run_activities(&self, run_id: RunId) -> Result<Vec<AgentActivityRecord>> {
        (**self).load_run_activities(run_id)
    }
    fn load_run_attempts(&self, run_id: RunId) -> Result<Vec<AgentRunAttemptRecord>> {
        (**self).load_run_attempts(run_id)
    }
    fn load_run_context_checkpoint(
        &self,
        run_id: RunId,
    ) -> Result<Option<DurableRunContextCheckpoint>> {
        (**self).load_run_context_checkpoint(run_id)
    }
    fn load_run_execution_state(&self, run_id: RunId) -> Result<Option<AgentExecutionStateRecord>> {
        (**self).load_run_execution_state(run_id)
    }
    fn load_run_interactions(&self, run_id: RunId) -> Result<Vec<AgentInteractionRecord>> {
        (**self).load_run_interactions(run_id)
    }
    fn load_run_message_content_range(
        &self,
        run_id: RunId,
        message_ordinal: u64,
        byte_offset: u64,
        length: usize,
    ) -> Result<Vec<u8>> {
        (**self).load_run_message_content_range(run_id, message_ordinal, byte_offset, length)
    }
    fn load_run_messages(&self, run_id: RunId) -> Result<Vec<DurableRunMessage>> {
        (**self).load_run_messages(run_id)
    }
    fn load_run_plan(&self, run_id: RunId) -> Result<AgentPlan> {
        (**self).load_run_plan(run_id)
    }
    fn load_run_runtime_config(&self, run_id: RunId) -> Result<Option<DurableRunRuntimeConfig>> {
        (**self).load_run_runtime_config(run_id)
    }
    fn load_run_summary(&self, run_id: RunId) -> Result<Option<DurableRunSummary>> {
        (**self).load_run_summary(run_id)
    }
    fn next_run_message_fragment_position(
        &self,
        run_id: RunId,
        message_ordinal: u64,
    ) -> Result<(u64, u64)> {
        (**self).next_run_message_fragment_position(run_id, message_ordinal)
    }
    fn save_recovery_updates(
        &self,
        summaries: &BTreeMap<RunId, DurableRunSummary>,
        feed: &DurableFeedState,
    ) -> Result<()> {
        (**self).save_recovery_updates(summaries, feed)
    }
    fn save_run_checkpoint(&self, write: DurableRunCheckpointWrite<'_>) -> Result<()> {
        (**self).save_run_checkpoint(write)
    }
    fn save_run_checkpoint_with_project_manager_wait(
        &self,
        write: DurableRunCheckpointWrite<'_>,
        wait: &ProjectManagerWaitRecord,
    ) -> Result<()> {
        (**self).save_run_checkpoint_with_project_manager_wait(write, wait)
    }
}

pub trait FilesystemRepository: Send + Sync {
    fn list_filesystem_sessions(&self) -> Result<Vec<AgentSessionId>>;
    fn load_filesystem_changes_page(
        &self,
        session_id: AgentSessionId,
        after: Option<EventSequence>,
        limit: usize,
    ) -> Result<DurableFilesystemChangesPage>;
    fn load_filesystem_record(
        &self,
        session_id: AgentSessionId,
    ) -> Result<Option<DurableFilesystemRecord>>;
    fn save_filesystem_changes(
        &self,
        session_id: AgentSessionId,
        next_sequence: EventSequence,
        changes: &[SessionFilesystemChange],
    ) -> Result<()>;
}

impl FilesystemRepository for FilePersistence {
    fn list_filesystem_sessions(&self) -> Result<Vec<AgentSessionId>> {
        FilePersistence::list_filesystem_sessions(self)
    }
    fn load_filesystem_changes_page(
        &self,
        session_id: AgentSessionId,
        after: Option<EventSequence>,
        limit: usize,
    ) -> Result<DurableFilesystemChangesPage> {
        FilePersistence::load_filesystem_changes_page(self, session_id, after, limit)
    }
    fn load_filesystem_record(
        &self,
        session_id: AgentSessionId,
    ) -> Result<Option<DurableFilesystemRecord>> {
        FilePersistence::load_filesystem_record(self, session_id)
    }
    fn save_filesystem_changes(
        &self,
        session_id: AgentSessionId,
        next_sequence: EventSequence,
        changes: &[SessionFilesystemChange],
    ) -> Result<()> {
        FilePersistence::save_filesystem_changes(self, session_id, next_sequence, changes)
    }
}

impl<T: FilesystemRepository + ?Sized> FilesystemRepository for Arc<T> {
    fn list_filesystem_sessions(&self) -> Result<Vec<AgentSessionId>> {
        (**self).list_filesystem_sessions()
    }
    fn load_filesystem_changes_page(
        &self,
        session_id: AgentSessionId,
        after: Option<EventSequence>,
        limit: usize,
    ) -> Result<DurableFilesystemChangesPage> {
        (**self).load_filesystem_changes_page(session_id, after, limit)
    }
    fn load_filesystem_record(
        &self,
        session_id: AgentSessionId,
    ) -> Result<Option<DurableFilesystemRecord>> {
        (**self).load_filesystem_record(session_id)
    }
    fn save_filesystem_changes(
        &self,
        session_id: AgentSessionId,
        next_sequence: EventSequence,
        changes: &[SessionFilesystemChange],
    ) -> Result<()> {
        (**self).save_filesystem_changes(session_id, next_sequence, changes)
    }
}

pub trait FeedRepository: Send + Sync {
    fn load_feed_events_since(
        &self,
        session_id: Option<AgentSessionId>,
        after_sequence: Option<EventSequence>,
    ) -> Result<Vec<ServerEventEnvelope>>;
    fn load_feed_session_cursor(
        &self,
        session_id: AgentSessionId,
    ) -> Result<Option<DurableFeedSessionCursor>>;
    fn load_feed_workspace_cursor(
        &self,
        workspace_id: WorkspaceId,
    ) -> Result<Option<DurableFeedWorkspaceCursor>>;
    fn load_feed_workspace_events_since(
        &self,
        workspace_id: WorkspaceId,
        after_sequence: Option<EventSequence>,
    ) -> Result<Vec<WorkspaceFeedEvent>>;
    fn load_feed_header(&self) -> Result<Option<DurableFeedHeader>>;
    fn load_recent_feed_events(
        &self,
        session_id: AgentSessionId,
        limit: usize,
    ) -> Result<Vec<ServerEventEnvelope>>;
}

impl FeedRepository for FilePersistence {
    fn load_feed_events_since(
        &self,
        session_id: Option<AgentSessionId>,
        after_sequence: Option<EventSequence>,
    ) -> Result<Vec<ServerEventEnvelope>> {
        FilePersistence::load_feed_events_since(self, session_id, after_sequence)
    }
    fn load_feed_session_cursor(
        &self,
        session_id: AgentSessionId,
    ) -> Result<Option<DurableFeedSessionCursor>> {
        FilePersistence::load_feed_session_cursor(self, session_id)
    }
    fn load_feed_workspace_cursor(
        &self,
        workspace_id: WorkspaceId,
    ) -> Result<Option<DurableFeedWorkspaceCursor>> {
        FilePersistence::load_feed_workspace_cursor(self, workspace_id)
    }
    fn load_feed_workspace_events_since(
        &self,
        workspace_id: WorkspaceId,
        after_sequence: Option<EventSequence>,
    ) -> Result<Vec<WorkspaceFeedEvent>> {
        FilePersistence::load_feed_workspace_events_since(self, workspace_id, after_sequence)
    }
    fn load_feed_header(&self) -> Result<Option<DurableFeedHeader>> {
        FilePersistence::load_feed_header(self)
    }
    fn load_recent_feed_events(
        &self,
        session_id: AgentSessionId,
        limit: usize,
    ) -> Result<Vec<ServerEventEnvelope>> {
        FilePersistence::load_recent_feed_events(self, session_id, limit)
    }
}

impl<T: FeedRepository + ?Sized> FeedRepository for Arc<T> {
    fn load_feed_events_since(
        &self,
        session_id: Option<AgentSessionId>,
        after_sequence: Option<EventSequence>,
    ) -> Result<Vec<ServerEventEnvelope>> {
        (**self).load_feed_events_since(session_id, after_sequence)
    }
    fn load_feed_session_cursor(
        &self,
        session_id: AgentSessionId,
    ) -> Result<Option<DurableFeedSessionCursor>> {
        (**self).load_feed_session_cursor(session_id)
    }
    fn load_feed_workspace_cursor(
        &self,
        workspace_id: WorkspaceId,
    ) -> Result<Option<DurableFeedWorkspaceCursor>> {
        (**self).load_feed_workspace_cursor(workspace_id)
    }
    fn load_feed_workspace_events_since(
        &self,
        workspace_id: WorkspaceId,
        after_sequence: Option<EventSequence>,
    ) -> Result<Vec<WorkspaceFeedEvent>> {
        (**self).load_feed_workspace_events_since(workspace_id, after_sequence)
    }
    fn load_feed_header(&self) -> Result<Option<DurableFeedHeader>> {
        (**self).load_feed_header()
    }
    fn load_recent_feed_events(
        &self,
        session_id: AgentSessionId,
        limit: usize,
    ) -> Result<Vec<ServerEventEnvelope>> {
        (**self).load_recent_feed_events(session_id, limit)
    }
}

pub trait ProjectRepository: Send + Sync {
    fn accept_agent_message(
        &self,
        request_id: RequestId,
        draft: &AgentMessageDraft,
    ) -> Result<loom_core::AgentMessageRecord>;
    fn begin_project_cancellation_cascade(
        &self,
        cascade: &ProjectCancellationCascadeRecord,
    ) -> Result<ProjectCancellationCascadeRecord>;
    fn claim_project_manager_wait(
        &self,
        wait_id: ProjectManagerWaitId,
        updated_at: Timestamp,
    ) -> Result<bool>;
    fn complete_project_cancellation_cascade(
        &self,
        project_id: ProjectId,
        root_task_id: TaskId,
    ) -> Result<bool>;
    fn create_project_child(
        &self,
        request_id: RequestId,
        child_snapshot: &AgentSessionSnapshot,
        session_next_sequence: EventSequence,
        task: &DelegatedTaskRecord,
    ) -> Result<DelegatedTaskRecord>;
    fn create_project_child_with_worktree(
        &self,
        request_id: RequestId,
        child_snapshot: &AgentSessionSnapshot,
        session_next_sequence: EventSequence,
        task: &DelegatedTaskRecord,
        worktree: &ProjectWorktreeRecord,
    ) -> Result<DelegatedTaskRecord>;
    fn has_pending_project_cancellation_cascade(&self, project_id: ProjectId) -> Result<bool>;
    fn list_agent_messages(
        &self,
        project_id: ProjectId,
        session_id: AgentSessionId,
        after: u64,
        limit: usize,
    ) -> Result<Vec<loom_core::AgentMessageRecord>>;
    fn list_pending_project_cancellation_cascades(
        &self,
    ) -> Result<Vec<ProjectCancellationCascadeRecord>>;
    fn list_project_tasks(&self, project_id: ProjectId) -> Result<Vec<DelegatedTaskRecord>>;
    fn list_project_manager_waits_by_child(
        &self,
        child_task_id: TaskId,
    ) -> Result<Vec<ProjectManagerWaitRecord>>;
    fn list_unfinished_project_manager_waits(&self) -> Result<Vec<ProjectManagerWaitRecord>>;
    fn load_delegated_task(&self, task_id: TaskId) -> Result<Option<DelegatedTaskRecord>>;
    fn load_delegated_task_for_target(
        &self,
        target_session_id: AgentSessionId,
    ) -> Result<Option<DelegatedTaskRecord>>;
    fn load_agent_message_by_request(
        &self,
        request_id: RequestId,
    ) -> Result<Option<loom_core::AgentMessageRecord>>;
    fn load_project_child_by_request(
        &self,
        request_id: RequestId,
        expected_project_id: ProjectId,
        expected_requester: AgentSessionId,
        child_name: &str,
        spec: &DelegatedTaskSpec,
    ) -> Result<Option<DelegatedTaskRecord>>;
    fn load_project_manager_wait(
        &self,
        wait_id: ProjectManagerWaitId,
    ) -> Result<Option<ProjectManagerWaitRecord>>;
    fn load_project_snapshot(&self, project_id: ProjectId) -> Result<Option<ProjectSnapshot>>;
    fn load_project_snapshot_for_session(
        &self,
        session_id: AgentSessionId,
    ) -> Result<Option<ProjectSnapshot>>;
    fn load_project_worktree_by_task(
        &self,
        task_id: TaskId,
    ) -> Result<Option<ProjectWorktreeRecord>>;
    fn load_session_projection_read(
        &self,
        session_id: AgentSessionId,
    ) -> Result<DurableSessionProjectionRead>;
    fn save_project_worktree(&self, worktree: &ProjectWorktreeRecord) -> Result<()>;
    fn transition_project_manager_wait(
        &self,
        wait_id: ProjectManagerWaitId,
        expected_status: ProjectManagerWaitStatus,
        next_status: ProjectManagerWaitStatus,
        result_summary: Option<&str>,
        updated_at: Timestamp,
    ) -> Result<bool>;
    fn update_delegated_task_status(
        &self,
        task_id: TaskId,
        status: DelegatedTaskStatus,
        updated_at: Timestamp,
    ) -> Result<bool>;
    fn update_delegated_task_status_if_queued(
        &self,
        task_id: TaskId,
        status: DelegatedTaskStatus,
        updated_at: Timestamp,
    ) -> Result<bool>;
}

impl ProjectRepository for FilePersistence {
    fn accept_agent_message(
        &self,
        request_id: RequestId,
        draft: &AgentMessageDraft,
    ) -> Result<loom_core::AgentMessageRecord> {
        FilePersistence::accept_agent_message(self, request_id, draft)
    }
    fn begin_project_cancellation_cascade(
        &self,
        cascade: &ProjectCancellationCascadeRecord,
    ) -> Result<ProjectCancellationCascadeRecord> {
        FilePersistence::begin_project_cancellation_cascade(self, cascade)
    }
    fn claim_project_manager_wait(
        &self,
        wait_id: ProjectManagerWaitId,
        updated_at: Timestamp,
    ) -> Result<bool> {
        FilePersistence::claim_project_manager_wait(self, wait_id, updated_at)
    }
    fn complete_project_cancellation_cascade(
        &self,
        project_id: ProjectId,
        root_task_id: TaskId,
    ) -> Result<bool> {
        FilePersistence::complete_project_cancellation_cascade(self, project_id, root_task_id)
    }
    fn create_project_child(
        &self,
        request_id: RequestId,
        child_snapshot: &AgentSessionSnapshot,
        session_next_sequence: EventSequence,
        task: &DelegatedTaskRecord,
    ) -> Result<DelegatedTaskRecord> {
        FilePersistence::create_project_child(
            self,
            request_id,
            child_snapshot,
            session_next_sequence,
            task,
        )
    }
    fn create_project_child_with_worktree(
        &self,
        request_id: RequestId,
        child_snapshot: &AgentSessionSnapshot,
        session_next_sequence: EventSequence,
        task: &DelegatedTaskRecord,
        worktree: &ProjectWorktreeRecord,
    ) -> Result<DelegatedTaskRecord> {
        FilePersistence::create_project_child_with_worktree(
            self,
            request_id,
            child_snapshot,
            session_next_sequence,
            task,
            worktree,
        )
    }
    fn has_pending_project_cancellation_cascade(&self, project_id: ProjectId) -> Result<bool> {
        FilePersistence::has_pending_project_cancellation_cascade(self, project_id)
    }
    fn list_agent_messages(
        &self,
        project_id: ProjectId,
        session_id: AgentSessionId,
        after: u64,
        limit: usize,
    ) -> Result<Vec<loom_core::AgentMessageRecord>> {
        FilePersistence::list_agent_messages(self, project_id, session_id, after, limit)
    }
    fn list_pending_project_cancellation_cascades(
        &self,
    ) -> Result<Vec<ProjectCancellationCascadeRecord>> {
        FilePersistence::list_pending_project_cancellation_cascades(self)
    }
    fn list_project_tasks(&self, project_id: ProjectId) -> Result<Vec<DelegatedTaskRecord>> {
        FilePersistence::list_project_tasks(self, project_id)
    }
    fn list_project_manager_waits_by_child(
        &self,
        child_task_id: TaskId,
    ) -> Result<Vec<ProjectManagerWaitRecord>> {
        FilePersistence::list_project_manager_waits_by_child(self, child_task_id)
    }
    fn list_unfinished_project_manager_waits(&self) -> Result<Vec<ProjectManagerWaitRecord>> {
        FilePersistence::list_unfinished_project_manager_waits(self)
    }
    fn load_delegated_task(&self, task_id: TaskId) -> Result<Option<DelegatedTaskRecord>> {
        FilePersistence::load_delegated_task(self, task_id)
    }
    fn load_delegated_task_for_target(
        &self,
        target_session_id: AgentSessionId,
    ) -> Result<Option<DelegatedTaskRecord>> {
        FilePersistence::load_delegated_task_for_target(self, target_session_id)
    }
    fn load_agent_message_by_request(
        &self,
        request_id: RequestId,
    ) -> Result<Option<loom_core::AgentMessageRecord>> {
        FilePersistence::load_agent_message_by_request(self, request_id)
    }
    fn load_project_child_by_request(
        &self,
        request_id: RequestId,
        expected_project_id: ProjectId,
        expected_requester: AgentSessionId,
        child_name: &str,
        spec: &DelegatedTaskSpec,
    ) -> Result<Option<DelegatedTaskRecord>> {
        FilePersistence::load_project_child_by_request(
            self,
            request_id,
            expected_project_id,
            expected_requester,
            child_name,
            spec,
        )
    }
    fn load_project_manager_wait(
        &self,
        wait_id: ProjectManagerWaitId,
    ) -> Result<Option<ProjectManagerWaitRecord>> {
        FilePersistence::load_project_manager_wait(self, wait_id)
    }
    fn load_project_snapshot(&self, project_id: ProjectId) -> Result<Option<ProjectSnapshot>> {
        FilePersistence::load_project_snapshot(self, project_id)
    }
    fn load_project_snapshot_for_session(
        &self,
        session_id: AgentSessionId,
    ) -> Result<Option<ProjectSnapshot>> {
        FilePersistence::load_project_snapshot_for_session(self, session_id)
    }
    fn load_project_worktree_by_task(
        &self,
        task_id: TaskId,
    ) -> Result<Option<ProjectWorktreeRecord>> {
        FilePersistence::load_project_worktree_by_task(self, task_id)
    }
    fn load_session_projection_read(
        &self,
        session_id: AgentSessionId,
    ) -> Result<DurableSessionProjectionRead> {
        FilePersistence::load_session_projection_read(self, session_id)
    }
    fn save_project_worktree(&self, worktree: &ProjectWorktreeRecord) -> Result<()> {
        FilePersistence::save_project_worktree(self, worktree)
    }
    fn transition_project_manager_wait(
        &self,
        wait_id: ProjectManagerWaitId,
        expected_status: ProjectManagerWaitStatus,
        next_status: ProjectManagerWaitStatus,
        result_summary: Option<&str>,
        updated_at: Timestamp,
    ) -> Result<bool> {
        FilePersistence::transition_project_manager_wait(
            self,
            wait_id,
            expected_status,
            next_status,
            result_summary,
            updated_at,
        )
    }
    fn update_delegated_task_status(
        &self,
        task_id: TaskId,
        status: DelegatedTaskStatus,
        updated_at: Timestamp,
    ) -> Result<bool> {
        FilePersistence::update_delegated_task_status(self, task_id, status, updated_at)
    }
    fn update_delegated_task_status_if_queued(
        &self,
        task_id: TaskId,
        status: DelegatedTaskStatus,
        updated_at: Timestamp,
    ) -> Result<bool> {
        FilePersistence::update_delegated_task_status_if_queued(self, task_id, status, updated_at)
    }
}

impl<T: ProjectRepository + ?Sized> ProjectRepository for Arc<T> {
    fn accept_agent_message(
        &self,
        request_id: RequestId,
        draft: &AgentMessageDraft,
    ) -> Result<loom_core::AgentMessageRecord> {
        (**self).accept_agent_message(request_id, draft)
    }
    fn begin_project_cancellation_cascade(
        &self,
        cascade: &ProjectCancellationCascadeRecord,
    ) -> Result<ProjectCancellationCascadeRecord> {
        (**self).begin_project_cancellation_cascade(cascade)
    }
    fn claim_project_manager_wait(
        &self,
        wait_id: ProjectManagerWaitId,
        updated_at: Timestamp,
    ) -> Result<bool> {
        (**self).claim_project_manager_wait(wait_id, updated_at)
    }
    fn complete_project_cancellation_cascade(
        &self,
        project_id: ProjectId,
        root_task_id: TaskId,
    ) -> Result<bool> {
        (**self).complete_project_cancellation_cascade(project_id, root_task_id)
    }
    fn create_project_child(
        &self,
        request_id: RequestId,
        child_snapshot: &AgentSessionSnapshot,
        session_next_sequence: EventSequence,
        task: &DelegatedTaskRecord,
    ) -> Result<DelegatedTaskRecord> {
        (**self).create_project_child(request_id, child_snapshot, session_next_sequence, task)
    }
    fn create_project_child_with_worktree(
        &self,
        request_id: RequestId,
        child_snapshot: &AgentSessionSnapshot,
        session_next_sequence: EventSequence,
        task: &DelegatedTaskRecord,
        worktree: &ProjectWorktreeRecord,
    ) -> Result<DelegatedTaskRecord> {
        (**self).create_project_child_with_worktree(
            request_id,
            child_snapshot,
            session_next_sequence,
            task,
            worktree,
        )
    }
    fn has_pending_project_cancellation_cascade(&self, project_id: ProjectId) -> Result<bool> {
        (**self).has_pending_project_cancellation_cascade(project_id)
    }
    fn list_agent_messages(
        &self,
        project_id: ProjectId,
        session_id: AgentSessionId,
        after: u64,
        limit: usize,
    ) -> Result<Vec<loom_core::AgentMessageRecord>> {
        (**self).list_agent_messages(project_id, session_id, after, limit)
    }
    fn list_pending_project_cancellation_cascades(
        &self,
    ) -> Result<Vec<ProjectCancellationCascadeRecord>> {
        (**self).list_pending_project_cancellation_cascades()
    }
    fn list_project_tasks(&self, project_id: ProjectId) -> Result<Vec<DelegatedTaskRecord>> {
        (**self).list_project_tasks(project_id)
    }
    fn list_project_manager_waits_by_child(
        &self,
        child_task_id: TaskId,
    ) -> Result<Vec<ProjectManagerWaitRecord>> {
        (**self).list_project_manager_waits_by_child(child_task_id)
    }
    fn list_unfinished_project_manager_waits(&self) -> Result<Vec<ProjectManagerWaitRecord>> {
        (**self).list_unfinished_project_manager_waits()
    }
    fn load_delegated_task(&self, task_id: TaskId) -> Result<Option<DelegatedTaskRecord>> {
        (**self).load_delegated_task(task_id)
    }
    fn load_delegated_task_for_target(
        &self,
        target_session_id: AgentSessionId,
    ) -> Result<Option<DelegatedTaskRecord>> {
        (**self).load_delegated_task_for_target(target_session_id)
    }
    fn load_agent_message_by_request(
        &self,
        request_id: RequestId,
    ) -> Result<Option<loom_core::AgentMessageRecord>> {
        (**self).load_agent_message_by_request(request_id)
    }
    fn load_project_child_by_request(
        &self,
        request_id: RequestId,
        expected_project_id: ProjectId,
        expected_requester: AgentSessionId,
        child_name: &str,
        spec: &DelegatedTaskSpec,
    ) -> Result<Option<DelegatedTaskRecord>> {
        (**self).load_project_child_by_request(
            request_id,
            expected_project_id,
            expected_requester,
            child_name,
            spec,
        )
    }
    fn load_project_manager_wait(
        &self,
        wait_id: ProjectManagerWaitId,
    ) -> Result<Option<ProjectManagerWaitRecord>> {
        (**self).load_project_manager_wait(wait_id)
    }
    fn load_project_snapshot(&self, project_id: ProjectId) -> Result<Option<ProjectSnapshot>> {
        (**self).load_project_snapshot(project_id)
    }
    fn load_project_snapshot_for_session(
        &self,
        session_id: AgentSessionId,
    ) -> Result<Option<ProjectSnapshot>> {
        (**self).load_project_snapshot_for_session(session_id)
    }
    fn load_project_worktree_by_task(
        &self,
        task_id: TaskId,
    ) -> Result<Option<ProjectWorktreeRecord>> {
        (**self).load_project_worktree_by_task(task_id)
    }
    fn load_session_projection_read(
        &self,
        session_id: AgentSessionId,
    ) -> Result<DurableSessionProjectionRead> {
        (**self).load_session_projection_read(session_id)
    }
    fn save_project_worktree(&self, worktree: &ProjectWorktreeRecord) -> Result<()> {
        (**self).save_project_worktree(worktree)
    }
    fn transition_project_manager_wait(
        &self,
        wait_id: ProjectManagerWaitId,
        expected_status: ProjectManagerWaitStatus,
        next_status: ProjectManagerWaitStatus,
        result_summary: Option<&str>,
        updated_at: Timestamp,
    ) -> Result<bool> {
        (**self).transition_project_manager_wait(
            wait_id,
            expected_status,
            next_status,
            result_summary,
            updated_at,
        )
    }
    fn update_delegated_task_status(
        &self,
        task_id: TaskId,
        status: DelegatedTaskStatus,
        updated_at: Timestamp,
    ) -> Result<bool> {
        (**self).update_delegated_task_status(task_id, status, updated_at)
    }
    fn update_delegated_task_status_if_queued(
        &self,
        task_id: TaskId,
        status: DelegatedTaskStatus,
        updated_at: Timestamp,
    ) -> Result<bool> {
        (**self).update_delegated_task_status_if_queued(task_id, status, updated_at)
    }
}

pub trait Persistence:
    CatalogRepository
    + SessionRepository
    + RunRepository
    + FilesystemRepository
    + FeedRepository
    + ProjectRepository
{
}
impl<
    T: CatalogRepository
        + SessionRepository
        + RunRepository
        + FilesystemRepository
        + FeedRepository
        + ProjectRepository,
> Persistence for T
{
}
