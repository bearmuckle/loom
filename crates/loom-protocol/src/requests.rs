use super::*;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
#[allow(clippy::large_enum_variant)]
pub enum ControlRequest {
    Negotiate {
        client_version: ProtocolVersion,
        capabilities: CapabilitySet,
    },

    DiscoverCapabilities,

    GetWorkerNodeStatus,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
#[allow(clippy::large_enum_variant)]
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
#[allow(clippy::large_enum_variant)]
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
#[allow(clippy::large_enum_variant)]
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
#[allow(clippy::large_enum_variant)]
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

    ForkAgentSession {
        session_id: AgentSessionId,
        name: String,
    },
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
#[allow(clippy::large_enum_variant)]
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
#[allow(clippy::large_enum_variant)]
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
#[allow(clippy::large_enum_variant)]
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
#[allow(clippy::large_enum_variant)]
pub enum UsageRequest {
    GetRunUsage { run_id: RunId },

    GetSessionUsage { session_id: AgentSessionId },
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
#[allow(clippy::large_enum_variant)]
pub enum ContextRequest {
    InspectAgentContext { run_id: RunId },
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
#[allow(clippy::large_enum_variant)]
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
#[allow(clippy::large_enum_variant)]
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
#[allow(clippy::large_enum_variant)]
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
#[allow(clippy::large_enum_variant)]
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
}
