use super::*;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
#[allow(clippy::large_enum_variant)]
pub enum ControlResponse {
    Negotiated(NegotiationResult),

    Capabilities(NegotiationResult),

    WorkerNodeStatus(WorkerNodeStatus),
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
#[allow(clippy::large_enum_variant)]
pub enum WorkspaceResponse {
    WorkspaceCreated(WorkspaceRecord),

    Workspaces { workspaces: Vec<WorkspaceRecord> },

    WorkspaceRenamed(WorkspaceRecord),

    WorkspaceConfig(WorkspaceConfig),

    WorkspaceConfigUpdated,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
#[allow(clippy::large_enum_variant)]
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
#[allow(clippy::large_enum_variant)]
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
#[allow(clippy::large_enum_variant)]
pub enum SessionResponse {
    AgentSessions { sessions: Vec<AgentSessionSnapshot> },

    AgentSessionCreated(AgentSessionSnapshot),

    AgentSessionForked(AgentSessionSnapshot),

    AgentSession(AgentSessionSnapshot),

    AgentSessionSnapshot(AgentSessionSnapshotProjection),

    AgentSessionInitialState(AgentSessionInitialState),

    AgentSessionRenamed(AgentSessionSnapshot),

    AgentSessionArchived(AgentSessionSnapshot),

    ApprovalPolicy(loom_core::ApprovalPolicy),
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
#[allow(clippy::large_enum_variant)]
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
#[allow(clippy::large_enum_variant)]
pub enum RunResponse {
    AgentRunStarted(AgentRunSnapshot),

    AgentRun(AgentRunSnapshot),

    AgentRunSnapshot(AgentRunSnapshotProjection),

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
#[allow(clippy::large_enum_variant)]
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
#[allow(clippy::large_enum_variant)]
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
#[allow(clippy::large_enum_variant)]
pub enum ContextResponse {
    ContextInspection(ContextInspection),
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
#[allow(clippy::large_enum_variant)]
pub enum TerminalResponse {
    TerminalOpened(TerminalSnapshot),

    Terminal(TerminalSnapshot),

    TerminalEvents { events: Vec<TerminalEventRecord> },
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
#[allow(clippy::large_enum_variant)]
pub enum TaskResponse {
    TaskStarted(TaskSnapshot),

    Tasks { tasks: Vec<TaskSnapshot> },

    Task(TaskSnapshot),

    TaskEvents { events: Vec<TaskEventRecord> },

    TaskEvidence { evidence: Vec<TaskEvidenceLink> },
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
#[allow(clippy::large_enum_variant)]
pub enum ProjectResponse {
    ProjectSnapshot(ProjectSnapshot),

    ProjectChildCreated {
        task: DelegatedTaskRecord,
        child: ProjectAgentRecord,
    },

    ProjectAgentMessageAccepted(AgentMessageRecord),

    ProjectAgentMessages {
        messages: Vec<AgentMessageRecord>,
        next_after_project_sequence: Option<u64>,
    },

    ProjectChildControlled {
        task: DelegatedTaskRecord,
        run: Option<AgentRunSnapshot>,
    },

    ProjectChildReview {
        worktree: ProjectWorktreeRecord,
        status: GitRepositoryStatus,
        diff: GitDiff,
    },

    ProjectChildWorktreeUpdated(ProjectWorktreeRecord),
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
#[allow(clippy::large_enum_variant)]
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
