use super::*;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum ControlResponse {
    Negotiated(NegotiationResult),

    Capabilities(NegotiationResult),

    WorkerNodeStatus(WorkerNodeStatus),
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

    ApprovalPolicy(loom_core::ApprovalPolicy),
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum EventsResponse {
    SessionEvents {
        events: Vec<ServerEventEnvelope>,
        #[serde(default)]
        stream_epoch: Option<String>,
    },

    WorkspaceEvents {
        workspace_id: WorkspaceId,
        events: Vec<WorkspaceFeedEvent>,
        #[serde(default)]
        stream_epoch: Option<String>,
    },

    SessionEventsSnapshot {
        session: AgentSessionSnapshot,
        events: Vec<ServerEventEnvelope>,
        oldest_sequence: EventSequence,
        latest_sequence: EventSequence,
        #[serde(default)]
        stream_epoch: Option<String>,
    },

    /// Returned when a workspace cursor is stale or the backend epoch changed. `sessions`
    /// is the current workspace catalog snapshot; `events` contains the retained workspace
    /// feed. `latest_sequence` is the latest event for this workspace, not the global head.
    WorkspaceEventsSnapshot {
        workspace_id: WorkspaceId,
        sessions: Vec<AgentSessionSnapshot>,
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
}
