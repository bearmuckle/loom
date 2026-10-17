use super::*;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum ControlResponse {
    Negotiated(NegotiationResult),

    Capabilities(NegotiationResult),

    WorkerNodeStatus(WorkerNodeStatus),

    ArchiveRetentionPolicy(ArchiveRetentionPolicy),
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum WorkspaceResponse {
    WorkspaceCreated(WorkspaceRecord),

    Workspaces { workspaces: Vec<WorkspaceRecord> },

    WorkspaceRenamed(WorkspaceRecord),

    WorkspaceConfig(WorkspaceConfig),

    WorkspaceConfigUpdated,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum RepositoryResponse {
    SessionRepositories {
        repositories: Vec<SessionRepository>,
    },

    SessionRepositoryAttached(SessionRepository),

    SessionRepositoryDetached,

    GitHubRepositories {
        repositories: Vec<GitHubRepository>,
    },

    ClonedRepositories {
        repositories: Vec<ClonedRepository>,
    },

    VcsStatus(GitRepositoryStatus),

    VcsDiff(GitDiff),

    VcsBranches {
        branches: Vec<GitBranch>,
    },

    VcsConflicts {
        paths: Vec<String>,
    },
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum FilesystemResponse {
    SessionDirectoryImported {
        path: String,
        repository: Option<SessionRepository>,
    },

    SessionDirectoryAttached {
        directory: SessionDirectory,
        repositories: Vec<SessionRepository>,
    },

    SessionDirectories {
        directories: Vec<SessionDirectory>,
    },

    SessionDirectoryDetached,

    SessionFilesystemSnapshot(SessionFilesystemSnapshot),

    SessionFilesystemChanges {
        changes: Vec<SessionFilesystemChange>,
        truncated: bool,
    },

    SessionFilesystemFile(SessionFilesystemFile),

    WorkspaceEditApplied(WorkspaceEditResult),

    WorkspaceControl(WorkspaceControl),

    CheckpointCreated(Checkpoint),

    CheckpointReverted(RevertResult),

    WorkspaceUndo(UndoResult),

    ContextFiles {
        files: Vec<ContextFileReference>,
    },
}

/// Which steps a session approval-policy change reaches.
///
/// A run resolves its approval policy once, when it is created, and evaluates
/// every step against that pinned copy, so this backend answers
/// [`ApprovalPolicyApplyScope::NextRun`]: a change made while a run executes is
/// stored for the session and reaches a run that starts later.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalPolicyApplyScope {
    /// The change takes effect for the running run's next step.
    RunningRun,

    /// The change is stored for the session and takes effect from the next run.
    ///
    /// A run that is already executing keeps the approval policy it pinned when
    /// it started, so its decisions do not change while it runs.
    #[default]
    NextRun,
}

/// The payload of [`SessionResponse::ApprovalPolicy`]: the stored policy and the
/// scope the change applies in.
///
/// The policy is flattened into the encoded body, which stays the bare policy
/// object a client that predates this type already deserializes; such a client
/// ignores `applies_from`, and a body that omits it reads as
/// [`ApprovalPolicyApplyScope::NextRun`], which is what a backend that predates
/// the field did with a running run.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct SessionApprovalPolicyUpdate {
    #[serde(flatten)]
    pub policy: loom_core::ApprovalPolicy,
    #[serde(default)]
    pub applies_from: ApprovalPolicyApplyScope,
}

impl SessionApprovalPolicyUpdate {
    /// Reports a stored session policy and the scope it applies in.
    pub fn new(policy: loom_core::ApprovalPolicy, applies_from: ApprovalPolicyApplyScope) -> Self {
        Self {
            policy,
            applies_from,
        }
    }
}

/// Readers of this response only need the policy it carries, so the payload
/// still dereferences to it.
impl std::ops::Deref for SessionApprovalPolicyUpdate {
    type Target = loom_core::ApprovalPolicy;

    fn deref(&self) -> &Self::Target {
        &self.policy
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum SessionResponse {
    AgentSessions { sessions: Vec<AgentSessionSnapshot> },

    AgentSessionCreated(AgentSessionSnapshot),

    AgentSessionForked(AgentSessionSnapshot),

    AgentSession(AgentSessionSnapshot),

    AgentSessionSnapshot(Box<AgentSessionSnapshotProjection>),

    AgentSessionInitialState(Box<AgentSessionInitialState>),

    AgentSessionRenamed(AgentSessionSnapshot),

    AgentSessionArchived(AgentSessionSnapshot),

    AgentSessionDeleted { session_id: AgentSessionId },

    ApprovalPolicy(SessionApprovalPolicyUpdate),
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum EventsResponse {
    SessionEvents {
        events: Vec<ServerEventEnvelope>,
        #[serde(default)]
        stream_epoch: Option<String>,
        /// The cursor a bounded batch stopped at, so the caller continues from
        /// it.
        ///
        /// An events response is bounded to one frame, so a batch larger than
        /// that bound is delivered as a sequence of pages. `Some(sequence)`
        /// means this batch ended at that event because the response reached
        /// its bound and more events follow: the caller asks again with
        /// `after_sequence: Some(sequence)` until `None` says the batch reached
        /// the latest journaled event. The field is additive (`serde` default),
        /// so a body that omits it decodes as `None` and a caller that ignores
        /// it still advances from the events it received.
        #[serde(default)]
        next_after_sequence: Option<EventSequence>,
    },

    WorkspaceEvents {
        workspace_id: WorkspaceId,
        events: Vec<WorkspaceFeedEvent>,
        #[serde(default)]
        stream_epoch: Option<String>,
        /// The cursor a bounded batch stopped at, so the caller continues from
        /// it.
        ///
        /// The workspace feed is bounded to one response frame for the same
        /// reason a session batch is: it grows with activity, so a feed larger
        /// than the bound arrives as a sequence of pages. `Some(sequence)` means
        /// this page ended at that event because the response reached its bound
        /// and more events follow: the caller asks again with
        /// `after_sequence: Some(sequence)` until `None` says the page reached
        /// the latest event of the feed. The field is additive (`serde`
        /// default), so a body that omits it decodes as `None` and a caller that
        /// ignores it still advances from the events it received.
        #[serde(default)]
        next_after_sequence: Option<EventSequence>,
    },

    SessionEventsSnapshot {
        session: AgentSessionSnapshot,
        /// The retained window, or its newest part when that window is larger
        /// than one response frame.
        ///
        /// A snapshot is bounded to a frame as well: an oversized window is
        /// delivered from its newest event backwards, and `oldest_sequence`
        /// names the first sequence actually carried rather than the oldest one
        /// the journal retains. Everything older is recovered by the durable
        /// rebuild the snapshot triggers, which is why dropping the oldest side
        /// is safe while dropping the newest would not be.
        events: Vec<ServerEventEnvelope>,
        oldest_sequence: EventSequence,
        latest_sequence: EventSequence,
        #[serde(default)]
        stream_epoch: Option<String>,
    },

    /// Returned when a workspace cursor is stale or the backend epoch changed. `sessions`
    /// is the current workspace catalog snapshot; `events` contains the retained workspace
    /// feed, or its newest part when the feed is larger than one response frame.
    /// `latest_sequence` is the latest event for this workspace, not the global head.
    WorkspaceEventsSnapshot {
        workspace_id: WorkspaceId,
        sessions: Vec<AgentSessionSnapshot>,
        /// The retained feed, or its newest part when it is larger than one
        /// response frame; `oldest_sequence` then names the first sequence
        /// actually carried. Older events come from the durable workspace
        /// catalog the snapshot carries instead.
        events: Vec<WorkspaceFeedEvent>,
        oldest_sequence: EventSequence,
        latest_sequence: EventSequence,
        #[serde(default)]
        stream_epoch: Option<String>,
    },
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum RunResponse {
    AgentRunStarted(AgentRunSnapshot),

    AgentRun(AgentRunSnapshot),

    AgentRunSnapshot(Box<AgentRunSnapshotProjection>),

    AgentRunMessagePage {
        run_id: RunId,
        messages: Vec<AgentRunMessageHeader>,
    },

    AgentRunTranscriptPage {
        run_id: RunId,
        messages: Vec<AgentRunTranscriptMessage>,
        next_before: Option<u64>,
        has_older: bool,
    },

    AgentRunMessageContentRange {
        run_id: RunId,
        message_ordinal: u64,
        byte_offset: u64,
        content: Vec<u8>,
    },

    RunCheckpoint(Checkpoint),
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum ProviderResponse {
    Models {
        models: Vec<ModelDescriptor>,
    },

    Providers {
        providers: Vec<ProviderSummary>,
    },

    ProviderConfigured,

    GitHubCopilotLoginStarted {
        login_id: String,
        user_code: String,
        verification_uri: String,
        expires_in: u64,
        interval: u64,
    },

    GitHubCopilotLoginStatus {
        status: GitHubCopilotLoginStatus,
    },

    GitHubRepositoryLoginStarted {
        login_id: String,
        user_code: String,
        verification_uri: String,
        expires_in: u64,
        interval: u64,
    },

    GitHubRepositoryLoginStatus {
        status: GitHubCopilotLoginStatus,
    },

    GitHubRepositoryAccess {
        connected: bool,
    },

    GitHubWriteAccess {
        enabled: bool,
    },

    ProviderHealth(ProviderHealth),
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum UsageResponse {
    RunUsage {
        usage: UsageSnapshot,
        provider: ProviderUsageSummary,
    },

    SessionUsage {
        usage: UsageSnapshot,
        provider: ProviderUsageSummary,
    },
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum ContextResponse {
    ContextInspection(ContextInspection),
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum TerminalResponse {
    TerminalOpened(TerminalSnapshot),

    Terminal(TerminalSnapshot),

    TerminalEvents { events: Vec<TerminalEventRecord> },
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum TaskResponse {
    TaskStarted(TaskSnapshot),

    Tasks { tasks: Vec<TaskSnapshot> },

    Task(TaskSnapshot),

    TaskEvents { events: Vec<TaskEventRecord> },

    TaskEvidence { evidence: Vec<TaskEvidenceLink> },
}

/// A project operation result.
///
/// The variants carrying a whole record or snapshot — a delegated task, a child
/// worktree, a run snapshot — box that payload: the enum is as large as its
/// largest variant, and every message is moved and cloned at that size. Boxing
/// keeps the JSON shape, because `serde` serializes `Box<T>` exactly like `T`.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum ProjectResponse {
    ProjectSnapshot(ProjectSnapshot),

    ProjectChildCreated {
        task: Box<DelegatedTaskRecord>,
        child: ProjectAgentRecord,
    },

    ProjectAgentMessageAccepted(AgentMessageRecord),

    ProjectAgentMessages {
        messages: Vec<AgentMessageRecord>,
        next_after_project_sequence: Option<u64>,
    },

    ProjectChildControlled {
        task: Box<DelegatedTaskRecord>,
        run: Option<Box<AgentRunSnapshot>>,
    },

    ProjectChildReview {
        worktree: Box<ProjectWorktreeRecord>,
        status: GitRepositoryStatus,
        diff: GitDiff,
    },

    ProjectChildWorktreeUpdated(Box<ProjectWorktreeRecord>),
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum ServerResponse {
    Control(ControlResponse),
    Workspace(WorkspaceResponse),
    Repository(RepositoryResponse),
    Filesystem(FilesystemResponse),
    Session(SessionResponse),
    Events(EventsResponse),
    Run(RunResponse),
    Provider(ProviderResponse),
    Usage(UsageResponse),
    Context(ContextResponse),
    Terminal(TerminalResponse),
    Task(TaskResponse),
    Project(ProjectResponse),
    Access(AccessResponse),
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum AccessResponse {
    /// The connection's identity. `None` for an unrestricted operator
    /// connection that is not bound to a person.
    CurrentParticipant {
        participant: Option<ParticipantIdentity>,
    },

    Participants {
        participants: Vec<ParticipantIdentity>,
    },

    SessionRoster {
        session_id: AgentSessionId,
        entries: Vec<RosterEntry>,
    },

    /// The grant after a grant, promotion, or demotion.
    SessionGrant(SessionGrant),

    SessionAccessRevoked {
        session_id: AgentSessionId,
        participant_id: ParticipantId,
    },
}
