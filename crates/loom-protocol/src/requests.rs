use super::*;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum ControlRequest {
    Negotiate {
        client_version: ProtocolVersion,
        capabilities: CapabilitySet,
    },

    DiscoverCapabilities,

    GetWorkerNodeStatus,

    GetArchiveRetentionPolicy,

    /// Replaces the archive-retention policy this worker applies. The policy is
    /// stored in durable state, so it survives a restart, and the sweep reads
    /// it at the start of every round.
    SetArchiveRetentionPolicy {
        policy: ArchiveRetentionPolicy,
    },
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum WorkspaceRequest {
    CreateWorkspace {
        name: String,
    },

    RegisterWorkspace {
        workspace: WorkspaceRecord,
    },

    ListWorkspaces,

    RenameWorkspace {
        workspace_id: WorkspaceId,
        name: String,
    },

    ListWorkspaceSessions {
        workspace_id: WorkspaceId,
        include_archived: bool,
    },

    CreateAgentSessionInWorkspace {
        workspace_id: WorkspaceId,
        name: String,
    },

    GetWorkspaceConfigForWorkspace {
        workspace_id: WorkspaceId,
    },

    SetWorkspaceConfigForWorkspace {
        workspace_id: WorkspaceId,
        config: WorkspaceConfig,
    },
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum RepositoryRequest {
    AttachSessionRepository {
        session_id: AgentSessionId,
        source: String,
        path: String,
        revision: Option<String>,
        /// Reuse the worker node's cached clone of `source` when one exists
        /// instead of cloning from the network again.
        #[serde(default)]
        reuse_local: bool,
    },

    ListSessionRepositories {
        session_id: AgentSessionId,
    },

    DetachSessionRepository {
        session_id: AgentSessionId,
        repository_id: RepositoryId,
    },

    /// Search repositories the authenticated GitHub account can access. The
    /// query must contain at least two characters; the worker fetches the
    /// account's owned, collaborator, and organization-member repositories and
    /// filters them locally.
    SearchGitHubRepositories {
        query: String,
        /// Session the search is for. A connection bound to a person must name
        /// a session it owns: only a session owner may browse with the
        /// worker's GitHub credential. Operator connections may omit it.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        session_id: Option<AgentSessionId>,
    },

    /// List repositories already cloned and cached on this worker node.
    ListClonedRepositories,

    GetSessionVcsStatus {
        session_id: AgentSessionId,
        repository_id: RepositoryId,
    },

    GetSessionVcsDiff {
        session_id: AgentSessionId,
        repository_id: RepositoryId,
        path: Option<String>,
        staged: bool,
    },

    GetSessionVcsBranches {
        session_id: AgentSessionId,
        repository_id: RepositoryId,
    },

    GetSessionVcsConflicts {
        session_id: AgentSessionId,
        repository_id: RepositoryId,
    },
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum FilesystemRequest {
    ImportSessionDirectory {
        session_id: AgentSessionId,
        source: String,
        path: String,
    },

    AttachSessionDirectory {
        session_id: AgentSessionId,
        source: String,
        path: String,
    },

    ListSessionDirectories {
        session_id: AgentSessionId,
    },

    DetachSessionDirectory {
        session_id: AgentSessionId,
        path: String,
    },

    GetSessionFilesystemSnapshot {
        session_id: AgentSessionId,
    },

    GetSessionFilesystemChanges {
        session_id: AgentSessionId,
        after_sequence: Option<EventSequence>,
    },

    ReadSessionFile {
        session_id: AgentSessionId,
        path: String,
    },

    ApplySessionFilesystemEdit {
        session_id: AgentSessionId,
        edit: WorkspaceEdit,
    },

    TakeSessionFilesystemControl {
        session_id: AgentSessionId,
        control: WorkspaceControl,
    },

    CreateSessionCheckpoint {
        session_id: AgentSessionId,
        label: String,
    },

    RevertSessionCheckpoint {
        session_id: AgentSessionId,
        checkpoint_id: loom_core::CheckpointId,
    },

    UndoSessionEdit {
        session_id: AgentSessionId,
    },

    GetSessionContextFiles {
        session_id: AgentSessionId,
    },
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum SessionRequest {
    SetSessionApprovalPolicy {
        session_id: AgentSessionId,
        policy: loom_core::ApprovalPolicy,
        auto_approve_actions: Option<bool>,
    },

    GetAgentSession {
        session_id: AgentSessionId,
    },

    GetAgentSessionSnapshot {
        session_id: AgentSessionId,
    },

    /// Returns session/run metadata without materializing the run transcript.
    /// Clients can load conversation history through the bounded message-page API.
    GetAgentSessionSnapshotMetadata {
        session_id: AgentSessionId,
    },

    GetAgentSessionInitialState {
        session_id: AgentSessionId,
    },

    RenameAgentSession {
        session_id: AgentSessionId,
        name: String,
    },

    ArchiveAgentSession {
        session_id: AgentSessionId,
    },

    /// Permanently removes an archived session and its stored history.
    DeleteAgentSession {
        session_id: AgentSessionId,
        force: bool,
    },

    ForkAgentSession {
        session_id: AgentSessionId,
        name: String,
    },
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum EventsRequest {
    GetSessionEvents {
        session_id: Option<AgentSessionId>,
        /// Workspace-wide event stream scope. Mutually exclusive with `session_id`.
        #[serde(default)]
        workspace_id: Option<WorkspaceId>,
        /// Last global event sequence processed for the selected scope. Workspace event
        /// sequences can have gaps because unrelated workspaces share the global counter.
        after_sequence: Option<EventSequence>,
        /// Backend-instance identity paired with `after_sequence`.
        #[serde(default)]
        stream_epoch: Option<String>,
    },

    GetRecentSessionEvents {
        session_id: AgentSessionId,
        limit: u32,
    },
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum RunRequest {
    StartSessionAgentRun {
        session_id: AgentSessionId,
        task: String,
        model: ModelId,
        system_instructions: Option<String>,
        repository_instructions: Option<String>,
    },

    StartSessionAgentRunWithOptions {
        session_id: AgentSessionId,
        task: String,
        model: ModelId,
        system_instructions: Option<String>,
        repository_instructions: Option<String>,
        limits: SessionLimits,
        context: ContextAssemblyOptions,
    },

    GetAgentRun {
        run_id: RunId,
    },

    GetAgentRunMessagePage {
        run_id: RunId,
        before_ordinal: Option<u64>,
        limit: u32,
    },

    GetAgentRunTranscriptPage {
        run_id: RunId,
        before_ordinal: Option<u64>,
        limit: u32,
    },

    GetAgentRunMessageContentRange {
        run_id: RunId,
        message_ordinal: u64,
        byte_offset: u64,
        length: u32,
    },

    GetAgentRunSnapshot {
        run_id: RunId,
    },

    GetRunCheckpoint {
        run_id: RunId,
    },

    ApproveAgentAction {
        run_id: RunId,
        attempt_id: loom_core::RunAttemptId,
        expected_control_revision: u64,
        tool_call_id: ToolCallId,
    },

    RejectAgentAction {
        run_id: RunId,
        attempt_id: loom_core::RunAttemptId,
        expected_control_revision: u64,
        tool_call_id: ToolCallId,
        reason: Option<String>,
    },

    SendAgentMessage {
        run_id: RunId,
        attempt_id: loom_core::RunAttemptId,
        expected_control_revision: u64,
        message: String,
    },

    InterruptAgentRun {
        run_id: RunId,
    },

    RetryAgentStep {
        run_id: RunId,
    },

    PauseAgentRun {
        run_id: RunId,
    },

    ResumeAgentRun {
        run_id: RunId,
    },

    RetryAgentFromCheckpoint {
        run_id: RunId,
        checkpoint_id: loom_core::CheckpointId,
    },

    AttachRunEvidence {
        run_id: RunId,
        evidence: Vec<loom_core::EvidenceLink>,
    },
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum ProviderRequest {
    ListModels,

    ListProviders,

    ConfigureGitHubCopilot {
        access_token: String,
    },

    ConfigureApiKeyProvider {
        provider_id: ProviderId,
        api_key: String,
    },

    StartGitHubCopilotLogin,

    GetGitHubCopilotLoginStatus {
        login_id: String,
    },

    ConfigureGitHubRepository {
        access_token: String,
    },

    StartGitHubRepositoryLogin,

    GetGitHubRepositoryLoginStatus {
        login_id: String,
    },

    GetGitHubRepositoryAccess,

    ConfigureGitHubWriteAccess {
        enabled: bool,
    },

    GetGitHubWriteAccess,

    DiscoverProviderModels {
        provider_id: ProviderId,
    },

    GetProviderHealth {
        provider_id: ProviderId,
    },
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum UsageRequest {
    GetRunUsage { run_id: RunId },

    GetSessionUsage { session_id: AgentSessionId },
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum ContextRequest {
    InspectAgentContext { run_id: RunId },
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum TerminalRequest {
    OpenSessionTerminal {
        session_id: AgentSessionId,
        command: String,
        args: Vec<String>,
        cwd: Option<String>,
    },

    WriteSessionTerminalInput {
        session_id: AgentSessionId,
        terminal_id: loom_core::TerminalId,
        input: String,
    },

    ResizeSessionTerminal {
        session_id: AgentSessionId,
        terminal_id: loom_core::TerminalId,
        rows: u16,
        columns: u16,
    },

    GetSessionTerminalEvents {
        session_id: AgentSessionId,
        terminal_id: loom_core::TerminalId,
        after_sequence: Option<EventSequence>,
    },

    CancelSessionTerminal {
        session_id: AgentSessionId,
        terminal_id: loom_core::TerminalId,
    },
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum TaskRequest {
    StartSessionTask {
        session_id: AgentSessionId,
        spec: TaskSpec,
    },

    ListSessionTasks {
        session_id: AgentSessionId,
    },

    GetSessionTask {
        session_id: AgentSessionId,
        task_id: loom_core::TaskId,
    },

    GetSessionTaskEvents {
        session_id: AgentSessionId,
        task_id: loom_core::TaskId,
        after_sequence: Option<EventSequence>,
    },

    CancelSessionTask {
        session_id: AgentSessionId,
        task_id: loom_core::TaskId,
    },

    GetSessionTaskEvidence {
        session_id: AgentSessionId,
        task_id: loom_core::TaskId,
    },
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum ProjectRequest {
    GetProjectSnapshot {
        project_id: ProjectId,
    },

    GetProjectSnapshotForSession {
        session_id: AgentSessionId,
    },

    SendProjectAgentMessage {
        message: AgentMessageDraft,
    },

    ListProjectAgentMessages {
        project_id: ProjectId,
        session_id: AgentSessionId,
        after_project_sequence: Option<u64>,
        limit: u32,
    },

    ControlProjectChild {
        project_id: ProjectId,
        manager_session_id: AgentSessionId,
        task_id: loom_core::TaskId,
        action: ProjectChildControlAction,
    },

    GetProjectChildReview {
        project_id: ProjectId,
        manager_session_id: AgentSessionId,
        task_id: loom_core::TaskId,
    },

    IntegrateProjectChild {
        project_id: ProjectId,
        manager_session_id: AgentSessionId,
        task_id: loom_core::TaskId,
        expected_parent_revision: String,
    },

    CleanupProjectChildWorktree {
        project_id: ProjectId,
        manager_session_id: AgentSessionId,
        task_id: loom_core::TaskId,
        disposition: ProjectWorktreeCleanupDisposition,
    },
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum ClientRequest {
    Control(ControlRequest),
    Workspace(WorkspaceRequest),
    Repository(RepositoryRequest),
    Filesystem(FilesystemRequest),
    Session(SessionRequest),
    Events(EventsRequest),
    Run(RunRequest),
    Provider(ProviderRequest),
    Usage(UsageRequest),
    Context(ContextRequest),
    Terminal(TerminalRequest),
    Task(TaskRequest),
    Project(ProjectRequest),
    Access(AccessRequest),
    Collaboration(CollaborationRequest),
}

/// Multi-user messages and run actions with server-side attribution.
///
/// The author of every request is the person (or operator) the connection
/// authenticated as; no request names it. Every request here needs the
/// participant role, except [`CollaborationRequest::GetRunRevision`], which
/// any reader may send.
///
/// Ordering and conflicts: run-scoped requests carry the [`RunRevision`] the
/// sender decided against. The server serializes them per run and accepts
/// one only when that revision is still current; otherwise it answers
/// [`RunActionOutcome::Conflict`] with the current revision and applies
/// nothing. See [`RunRevision`].
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum CollaborationRequest {
    /// Chat to the other people in the session. Stored in the session
    /// history, never added to model context, and never starts or continues
    /// a run.
    SendPeerMessage {
        session_id: AgentSessionId,
        text: String,
    },

    /// A follow-up direction to the agent of a run.
    DirectAgent {
        run_id: RunId,
        expected: RunRevision,
        text: String,
    },

    /// An answer to the question the agent is waiting on.
    AnswerAgentQuestion {
        run_id: RunId,
        expected: RunRevision,
        text: String,
    },

    /// Approve, reject, pause, resume, interrupt, or retry a run.
    ControlRun {
        run_id: RunId,
        expected: RunRevision,
        action: RunControlAction,
    },

    /// The run's current revision, to decide an action against.
    GetRunRevision { run_id: RunId },

    /// The session's retained attributed history entries, oldest first, so a
    /// client can rebuild peer chat and attribution after a reload. Pages
    /// follow `next_after_sequence`. Any reader may send it.
    ListSessionCollaboration {
        session_id: AgentSessionId,
        after_sequence: Option<EventSequence>,
        limit: u32,
    },
}

/// The most entries one [`CollaborationRequest::ListSessionCollaboration`]
/// page returns.
pub const MAX_SESSION_COLLABORATION_PAGE_SIZE: u32 = 500;

/// Multi-user identity, session access, and presence.
///
/// The caller's identity always comes from the credential its connection
/// authenticated with; no request names the caller. A participant is named by
/// its stable [`ParticipantId`], never by a display name.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum AccessRequest {
    /// The identity bound to this connection.
    GetCurrentParticipant,

    /// People the backend knows, so an owner can pick whom to grant access to.
    ListParticipants,

    /// One entry per person with access to the session: role and presence
    /// aggregated across that person's connections.
    GetSessionRoster { session_id: AgentSessionId },

    /// Grants a person access to a session. Owner only. A new grant always
    /// starts as [`SessionRole::Observer`]; promote it with
    /// [`AccessRequest::SetSessionRole`]. Granting an existing grantee is a
    /// no-op that keeps the current role.
    GrantSessionAccess {
        session_id: AgentSessionId,
        participant_id: ParticipantId,
    },

    /// Promotes or demotes an existing grant. Owner only. The last owner of a
    /// session cannot be demoted.
    SetSessionRole {
        session_id: AgentSessionId,
        participant_id: ParticipantId,
        role: SessionRole,
    },

    /// Removes a person's access. Owner only. Their in-flight requests and
    /// live event subscriptions for the session end; other people are not
    /// affected. The last owner of a session cannot be revoked.
    RevokeSessionAccess {
        session_id: AgentSessionId,
        participant_id: ParticipantId,
    },

    /// Provider credentials the caller may select: their own, plus the
    /// worker-wide default. Metadata only; secrets never leave the worker.
    ListProviderCredentials,

    /// Selects the provider credential later runs of the session use. Owner
    /// only, and only a credential the owner holds (or the worker default).
    /// A run that already started keeps the credential it pinned.
    SelectSessionCredential {
        session_id: AgentSessionId,
        credential_id: CredentialId,
    },

    /// The session's selected credential, so clients can show whose account
    /// a run will use before it starts.
    GetSessionCredential { session_id: AgentSessionId },

    /// The credential and model a run pinned when it started.
    GetRunCredential { run_id: RunId },

    /// Revokes one of the caller's own credentials, in every version. Runs
    /// pinned to it fail their next provider call with an authentication
    /// error instead of switching to another account.
    RevokeProviderCredential { credential_id: CredentialId },

    /// Asks the session's owners for access, or for a higher role. Needs only
    /// a connection bound to a person; no grant is required. A person has at
    /// most one pending request per session: asking again replaces the role
    /// and note of the pending one and keeps its id. Protocol 11.5.
    RequestSessionAccess {
        session_id: AgentSessionId,
        requested_role: SessionRole,
        /// Optional note for the owners, at most
        /// [`loom_core::MAX_ACCESS_REQUEST_NOTE_BYTES`] bytes.
        note: Option<String>,
    },

    /// Pending access requests of a session, oldest first. Owner only.
    ListSessionAccessRequests { session_id: AgentSessionId },

    /// Approves a pending request. Owner only. The requester is granted
    /// `role`, or the requested role when `role` is `None`.
    ApproveSessionAccessRequest {
        session_id: AgentSessionId,
        request_id: AccessRequestId,
        role: Option<SessionRole>,
    },

    /// Denies a pending request. Owner only. The requester's access is
    /// unchanged.
    DenySessionAccessRequest {
        session_id: AgentSessionId,
        request_id: AccessRequestId,
        reason: Option<String>,
    },

    /// Withdraws the caller's own pending request.
    WithdrawSessionAccessRequest {
        session_id: AgentSessionId,
        request_id: AccessRequestId,
    },
}
