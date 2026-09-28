use loom_core::{
    AgentMessageDraft, AgentMessageRecord, AgentSessionId, AgentSessionSnapshot, Capability,
    CapabilitySet, DelegatedTaskRecord, EventSequence, LoomError, ProjectAgentRecord, ProjectId,
    ProjectSnapshot, ProjectWorktreeCleanupDisposition, ProjectWorktreeRecord, ProtocolVersion,
    RepositoryId, RequestId, RunId, SessionEvent, SessionEventRecord, SessionLimits, ToolCallId,
    UsageSnapshot, WorkspaceId,
};
use loom_model::{
    ModelDescriptor, ModelId, ModelMessage, ProviderHealth, ProviderId, ProviderSummary,
    ProviderUsageSummary, ToolCall,
};
use serde::{Deserialize, Serialize};
use thiserror::Error;

mod activity;
mod agent;
mod context;
mod process;
mod tool;
mod vcs;
mod workspace;

pub use activity::{
    AgentActivityData, AgentActivityKind, AgentActivityRecord, AgentActivityStatus,
    FileActivityOperation,
};
pub use agent::{
    AgentEvent, AgentExecutionStateRecord, AgentInteractionKind, AgentInteractionRecord,
    AgentInteractionStatus, AgentPlan, AgentPlanStep, AgentRunAttemptRecord, AgentRunSnapshot,
    AgentRunState, AgentToolAttemptRecord, AgentToolAttemptState, AgentToolCallRecord,
    ApprovalDecision, ProjectJoinContinuation,
};
pub use context::{
    ContextAssemblyOptions, ContextBudget, ContextInspection, ContextItem, ContextItemKind,
    ContextSummary,
};
pub use process::{
    TaskArtifact, TaskEvent, TaskEventRecord, TaskEvidenceLink, TaskKind, TaskSnapshot, TaskSpec,
    TaskStatus, TerminalEvent, TerminalEventRecord, TerminalSnapshot, TerminalStatus,
    TerminalStream,
};
pub use tool::ToolResult;
pub use vcs::{
    GitBranch, GitDiff, GitDiffHunk, GitDiffLine, GitDiffLineKind, GitFileStatus,
    GitFileStatusKind, GitRepositoryStatus,
};
pub use workspace::{
    Checkpoint, CheckpointFile, ContextFileKind, ContextFileReference, GitHubRepository,
    MAX_PROJECT_AGENT_CONCURRENCY, MIN_PROJECT_AGENT_CONCURRENCY, RevertResult, SessionDirectory,
    SessionFilesystemChange, SessionFilesystemFile, SessionFilesystemSnapshot, SessionRepository,
    UndoResult, WorkerNodeConfig, WorkspaceChangeKind, WorkspaceConfig, WorkspaceControl,
    WorkspaceEdit, WorkspaceEditResult, WorkspaceEntry, WorkspaceEntryKind, WorkspaceRecord,
};

pub const CURRENT_PROTOCOL_VERSION: ProtocolVersion = ProtocolVersion::new(10, 0);
pub const MAX_AGENT_RUN_MESSAGE_PAGE_SIZE: u32 = 100;
pub const MAX_AGENT_RUN_MESSAGE_CONTENT_RANGE_BYTES: u32 = 256 * 1024;
pub const MAX_AGENT_RUN_TRANSCRIPT_PAGE_SIZE: u32 = 32;
pub const MAX_AGENT_RUN_TRANSCRIPT_MESSAGE_BYTES: u32 = 32 * 1024;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProjectChildControlAction {
    Continue,
    RetryFailedStep,
    Pause,
    Interrupt,
    Cancel,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct WorkerNodeResources {
    pub cpu_count: usize,
    /// System-wide CPU utilization sampled between status requests.
    #[serde(default)]
    pub cpu_usage_percent: Option<u8>,
    /// Used system memory as a percentage of total memory.
    #[serde(default)]
    pub memory_usage_percent: Option<u8>,
    pub memory_total_bytes: Option<u64>,
    pub memory_available_bytes: Option<u64>,
    pub disk_total_bytes: Option<u64>,
    pub disk_available_bytes: Option<u64>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct WorkerNodeStatus {
    pub node_id: String,
    pub name: String,
    pub online: bool,
    pub capabilities: CapabilitySet,
    pub resources: WorkerNodeResources,
}

#[cfg(test)]
mod worker_node_resource_tests {
    use super::WorkerNodeResources;

    #[test]
    fn missing_percentage_fields_default_for_older_peers() {
        let resources: WorkerNodeResources = serde_json::from_str(
            r#"{"cpu_count":4,"memory_total_bytes":8192,"memory_available_bytes":4096,"disk_total_bytes":null,"disk_available_bytes":null}"#,
        )
        .unwrap();
        assert_eq!(resources.cpu_usage_percent, None);
        assert_eq!(resources.memory_usage_percent, None);
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct AgentRunSnapshotProjection {
    pub run: AgentRunSnapshot,
    pub plan: Vec<AgentPlanStep>,
    pub messages: Vec<ModelMessage>,
    pub pending_approval: Option<ToolCall>,
    pub pending_input: Option<String>,
    pub usage: UsageSnapshot,
    #[serde(default)]
    pub activities: Vec<AgentActivityRecord>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct AgentRunMessageHeader {
    pub ordinal: u64,
    pub role: loom_model::MessageRole,
    pub content_bytes: u64,
    pub name: Option<String>,
    pub tool_call_id: Option<loom_core::ToolCallId>,
    pub tool_calls: Vec<ToolCall>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct AgentRunTranscriptMessage {
    pub ordinal: u64,
    pub message: ModelMessage,
    pub content_truncated: bool,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct AgentSessionSnapshotProjection {
    pub session: AgentSessionSnapshot,
    pub active_run: Option<AgentRunSnapshotProjection>,
    pub latest_sequence: EventSequence,
    #[serde(default)]
    pub approval_policy: loom_core::ApprovalPolicy,
    #[serde(default = "default_auto_approve_actions")]
    pub auto_approve_actions: bool,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct AgentSessionInitialState {
    pub projection: AgentSessionSnapshotProjection,
    pub cursor: EventSequence,
}

fn default_auto_approve_actions() -> bool {
    true
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct RequestEnvelope {
    pub protocol_version: ProtocolVersion,
    pub request_id: RequestId,
    pub request: ClientRequest,
}

impl RequestEnvelope {
    pub fn new(request: ClientRequest) -> Self {
        Self {
            protocol_version: CURRENT_PROTOCOL_VERSION,
            request_id: RequestId::new(),
            request,
        }
    }

    pub fn with_version(protocol_version: ProtocolVersion, request: ClientRequest) -> Self {
        Self {
            protocol_version,
            request_id: RequestId::new(),
            request,
        }
    }

    pub fn with_request_id(request_id: RequestId, request: ClientRequest) -> Self {
        Self {
            protocol_version: CURRENT_PROTOCOL_VERSION,
            request_id,
            request,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum ClientRequest {
    Negotiate {
        client_version: ProtocolVersion,
        capabilities: CapabilitySet,
    },
    DiscoverCapabilities,
    GetWorkerNodeStatus,
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
    AttachSessionRepository {
        session_id: AgentSessionId,
        source: String,
        path: String,
        revision: Option<String>,
    },
    ListSessionRepositories {
        session_id: AgentSessionId,
    },
    DetachSessionRepository {
        session_id: AgentSessionId,
        repository_id: RepositoryId,
    },
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
    ListGitHubRepositories,
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
    RenameAgentSession {
        session_id: AgentSessionId,
        name: String,
    },
    ArchiveAgentSession {
        session_id: AgentSessionId,
    },
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
    ForkAgentSession {
        session_id: AgentSessionId,
        name: String,
    },
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
    DiscoverProviderModels {
        provider_id: ProviderId,
    },
    GetProviderHealth {
        provider_id: ProviderId,
    },
    GetRunUsage {
        run_id: RunId,
    },
    GetSessionUsage {
        session_id: AgentSessionId,
    },
    InspectAgentContext {
        run_id: RunId,
    },
    AttachRunEvidence {
        run_id: RunId,
        evidence: Vec<loom_core::EvidenceLink>,
    },
}

impl ClientRequest {
    pub const fn required_capability(&self) -> Option<Capability> {
        match self {
            Self::Negotiate { .. } | Self::DiscoverCapabilities => None,
            Self::GetWorkerNodeStatus => Some(Capability::ReadWorkerNodeStatus),
            Self::CreateWorkspace { .. }
            | Self::RegisterWorkspace { .. }
            | Self::RenameWorkspace { .. } => Some(Capability::ManageWorkspaces),
            Self::ListWorkspaces | Self::ListWorkspaceSessions { .. } => {
                Some(Capability::ReadAgentSession)
            }
            Self::CreateAgentSessionInWorkspace { .. } => Some(Capability::CreateAgentSession),
            Self::GetWorkspaceConfigForWorkspace { .. } => Some(Capability::ReadWorkspaceConfig),
            Self::SetWorkspaceConfigForWorkspace { .. } => Some(Capability::ManageWorkspaces),
            Self::AttachSessionRepository { .. } | Self::DetachSessionRepository { .. } => {
                Some(Capability::ManageSessionRepositories)
            }
            Self::ImportSessionDirectory { .. }
            | Self::AttachSessionDirectory { .. }
            | Self::DetachSessionDirectory { .. } => Some(Capability::WriteSessionFilesystem),
            Self::ListGitHubRepositories => Some(Capability::BrowseGitHubRepositories),
            Self::ListSessionRepositories { .. } | Self::ListSessionDirectories { .. } => {
                Some(Capability::ReadSessionFilesystem)
            }
            Self::StartSessionAgentRun { .. } | Self::StartSessionAgentRunWithOptions { .. } => {
                Some(Capability::StartAgentRun)
            }
            Self::GetSessionFilesystemSnapshot { .. }
            | Self::GetSessionFilesystemChanges { .. }
            | Self::ReadSessionFile { .. }
            | Self::GetSessionContextFiles { .. } => Some(Capability::ReadSessionFilesystem),
            Self::ApplySessionFilesystemEdit { .. } => Some(Capability::WriteSessionFilesystem),
            Self::TakeSessionFilesystemControl { .. } => Some(Capability::WriteSessionFilesystem),
            Self::CreateSessionCheckpoint { .. }
            | Self::RevertSessionCheckpoint { .. }
            | Self::UndoSessionEdit { .. } => Some(Capability::ManageCheckpoints),
            Self::GetSessionVcsStatus { .. }
            | Self::GetSessionVcsBranches { .. }
            | Self::GetSessionVcsConflicts { .. } => Some(Capability::ReadVcsStatus),
            Self::GetSessionVcsDiff { .. } => Some(Capability::ReadVcsDiff),
            Self::OpenSessionTerminal { .. } => Some(Capability::OpenSessionTerminal),
            Self::WriteSessionTerminalInput { .. }
            | Self::ResizeSessionTerminal { .. }
            | Self::CancelSessionTerminal { .. } => Some(Capability::ControlSessionTerminal),
            Self::GetSessionTerminalEvents { .. } => Some(Capability::ControlSessionTerminal),
            Self::StartSessionTask { .. } => Some(Capability::StartSessionTask),
            Self::ListSessionTasks { .. }
            | Self::GetSessionTask { .. }
            | Self::GetSessionTaskEvents { .. } => Some(Capability::ReadSessionTask),
            Self::CancelSessionTask { .. } => Some(Capability::ControlSessionTask),
            Self::GetSessionTaskEvidence { .. } => Some(Capability::ReadSessionTaskEvidence),
            Self::SetSessionApprovalPolicy { .. } => Some(Capability::ConfigureApprovalPolicy),
            Self::GetAgentSession { .. }
            | Self::GetAgentSessionSnapshot { .. }
            | Self::GetAgentSessionSnapshotMetadata { .. }
            | Self::GetAgentSessionInitialState { .. } => Some(Capability::ReadAgentSession),
            Self::GetProjectSnapshot { .. } | Self::GetProjectSnapshotForSession { .. } => {
                Some(Capability::ReadProject)
            }
            Self::SendProjectAgentMessage { .. } => Some(Capability::SendProjectAgentMessage),
            Self::ListProjectAgentMessages { .. } => Some(Capability::ReadProjectAgentMessages),
            Self::ControlProjectChild { .. } => Some(Capability::ControlProjectChild),
            Self::GetProjectChildReview { .. } => Some(Capability::ReadProjectChildReview),
            Self::IntegrateProjectChild { .. } => Some(Capability::IntegrateProjectChild),
            Self::CleanupProjectChildWorktree { .. } => {
                Some(Capability::CleanupProjectChildWorktree)
            }
            Self::RenameAgentSession { .. } | Self::ArchiveAgentSession { .. } => {
                Some(Capability::ControlAgentSession)
            }
            Self::GetSessionEvents { .. } | Self::GetRecentSessionEvents { .. } => {
                Some(Capability::SubscribeSessionEvents)
            }
            Self::GetAgentRun { .. } | Self::GetAgentRunSnapshot { .. } => {
                Some(Capability::ReadAgentRun)
            }
            Self::GetAgentRunMessagePage { .. }
            | Self::GetAgentRunTranscriptPage { .. }
            | Self::GetAgentRunMessageContentRange { .. } => Some(Capability::ReadAgentRunMessages),
            Self::GetRunCheckpoint { .. } => Some(Capability::ReadAgentRun),
            Self::ApproveAgentAction { .. } | Self::RejectAgentAction { .. } => {
                Some(Capability::ApproveAgentAction)
            }
            Self::InterruptAgentRun { .. } | Self::RetryAgentStep { .. } => {
                Some(Capability::ControlAgentRun)
            }
            Self::SendAgentMessage { .. } => Some(Capability::ControlAgentRun),
            Self::PauseAgentRun { .. } => Some(Capability::PauseAgentRun),
            Self::ResumeAgentRun { .. } => Some(Capability::ResumeAgentRun),
            Self::RetryAgentFromCheckpoint { .. } => Some(Capability::RetryFromCheckpoint),
            Self::ForkAgentSession { .. } => Some(Capability::ForkAgentSession),
            Self::ListModels => None,
            Self::ListProviders => Some(Capability::ListProviders),
            Self::ConfigureGitHubCopilot { .. }
            | Self::ConfigureApiKeyProvider { .. }
            | Self::StartGitHubCopilotLogin
            | Self::GetGitHubCopilotLoginStatus { .. } => Some(Capability::ConfigureProviders),
            Self::DiscoverProviderModels { .. } => Some(Capability::ListProviders),
            Self::GetProviderHealth { .. } => Some(Capability::ReadProviderHealth),
            Self::GetRunUsage { .. } => Some(Capability::ReadUsage),
            Self::GetSessionUsage { .. } => Some(Capability::ReadUsage),
            Self::InspectAgentContext { .. } => Some(Capability::InspectContext),
            Self::AttachRunEvidence { .. } => Some(Capability::ControlAgentRun),
        }
    }

    pub const fn is_retryable_mutation(&self) -> bool {
        matches!(
            self,
            Self::CreateWorkspace { .. }
                | Self::RegisterWorkspace { .. }
                | Self::RenameWorkspace { .. }
                | Self::CreateAgentSessionInWorkspace { .. }
                | Self::ControlProjectChild { .. }
                | Self::IntegrateProjectChild { .. }
                | Self::CleanupProjectChildWorktree { .. }
                | Self::SetWorkspaceConfigForWorkspace { .. }
                | Self::AttachSessionRepository { .. }
                | Self::DetachSessionRepository { .. }
                | Self::AttachSessionDirectory { .. }
                | Self::DetachSessionDirectory { .. }
                | Self::StartSessionAgentRun { .. }
                | Self::StartSessionAgentRunWithOptions { .. }
                | Self::ApplySessionFilesystemEdit { .. }
                | Self::TakeSessionFilesystemControl { .. }
                | Self::CreateSessionCheckpoint { .. }
                | Self::RevertSessionCheckpoint { .. }
                | Self::UndoSessionEdit { .. }
                | Self::OpenSessionTerminal { .. }
                | Self::WriteSessionTerminalInput { .. }
                | Self::ResizeSessionTerminal { .. }
                | Self::CancelSessionTerminal { .. }
                | Self::StartSessionTask { .. }
                | Self::CancelSessionTask { .. }
                | Self::SetSessionApprovalPolicy { .. }
                | Self::RenameAgentSession { .. }
                | Self::ArchiveAgentSession { .. }
                | Self::ApproveAgentAction { .. }
                | Self::RejectAgentAction { .. }
                | Self::SendAgentMessage { .. }
                | Self::SendProjectAgentMessage { .. }
                | Self::InterruptAgentRun { .. }
                | Self::RetryAgentStep { .. }
                | Self::PauseAgentRun { .. }
                | Self::ResumeAgentRun { .. }
                | Self::RetryAgentFromCheckpoint { .. }
                | Self::ForkAgentSession { .. }
                | Self::ConfigureGitHubCopilot { .. }
                | Self::AttachRunEvidence { .. }
        )
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ResponseEnvelope {
    pub protocol_version: ProtocolVersion,
    pub request_id: RequestId,
    pub result: std::result::Result<ServerResponse, LoomError>,
}

impl ResponseEnvelope {
    pub fn success(request_id: RequestId, response: ServerResponse) -> Self {
        Self {
            protocol_version: CURRENT_PROTOCOL_VERSION,
            request_id,
            result: Ok(response),
        }
    }

    pub fn failure(request_id: RequestId, error: LoomError) -> Self {
        Self {
            protocol_version: CURRENT_PROTOCOL_VERSION,
            request_id,
            result: Err(error),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum ServerResponse {
    Negotiated(NegotiationResult),
    Capabilities(NegotiationResult),
    WorkerNodeStatus(WorkerNodeStatus),
    WorkspaceCreated(WorkspaceRecord),
    Workspaces {
        workspaces: Vec<WorkspaceRecord>,
    },
    WorkspaceRenamed(WorkspaceRecord),
    SessionRepositories {
        repositories: Vec<SessionRepository>,
    },
    SessionRepositoryAttached(SessionRepository),
    SessionRepositoryDetached,
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
    GitHubRepositories {
        repositories: Vec<GitHubRepository>,
    },
    AgentSessions {
        sessions: Vec<AgentSessionSnapshot>,
    },
    AgentSessionCreated(AgentSessionSnapshot),
    AgentSessionForked(AgentSessionSnapshot),
    AgentSession(AgentSessionSnapshot),
    AgentSessionSnapshot(AgentSessionSnapshotProjection),
    AgentSessionInitialState(AgentSessionInitialState),
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
    AgentSessionRenamed(AgentSessionSnapshot),
    AgentSessionArchived(AgentSessionSnapshot),
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
    ProviderHealth(ProviderHealth),
    RunUsage {
        usage: UsageSnapshot,
        provider: ProviderUsageSummary,
    },
    SessionUsage {
        usage: UsageSnapshot,
        provider: ProviderUsageSummary,
    },
    ContextInspection(ContextInspection),
    WorkspaceConfig(WorkspaceConfig),
    WorkspaceConfigUpdated,
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
    ApprovalPolicy(loom_core::ApprovalPolicy),
    TerminalOpened(TerminalSnapshot),
    Terminal(TerminalSnapshot),
    TerminalEvents {
        events: Vec<TerminalEventRecord>,
    },
    TaskStarted(TaskSnapshot),
    Tasks {
        tasks: Vec<TaskSnapshot>,
    },
    Task(TaskSnapshot),
    TaskEvents {
        events: Vec<TaskEventRecord>,
    },
    ContextFiles {
        files: Vec<ContextFileReference>,
    },
    VcsStatus(GitRepositoryStatus),
    VcsDiff(GitDiff),
    VcsBranches {
        branches: Vec<GitBranch>,
    },
    VcsConflicts {
        paths: Vec<String>,
    },
    TaskEvidence {
        evidence: Vec<TaskEvidenceLink>,
    },
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum GitHubCopilotLoginStatus {
    Pending,
    Configured,
    Failed { message: String },
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct NegotiationResult {
    pub protocol_version: ProtocolVersion,
    pub capabilities: CapabilitySet,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum ClientFrame {
    Request(Box<RequestEnvelope>),
    Cancel { request_id: RequestId },
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
#[allow(clippy::large_enum_variant)]
pub enum ServerFrame {
    Response(Box<ResponseEnvelope>),
    Event(ServerEventEnvelope),
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ServerEventEnvelope {
    pub protocol_version: ProtocolVersion,
    pub sequence: EventSequence,
    pub session_id: AgentSessionId,
    pub event: ServerEvent,
}

/// A change to workspace catalog or configuration state. Workspace events have
/// their own scope and never borrow a session ID.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct WorkspaceEventEnvelope {
    pub protocol_version: ProtocolVersion,
    pub sequence: EventSequence,
    pub workspace_id: WorkspaceId,
    pub event: WorkspaceEvent,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum WorkspaceEvent {
    Renamed { name: String },
    ConfigChanged { revision: u64 },
}

/// Unified entries in a workspace reconnect stream.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(untagged)]
#[allow(clippy::large_enum_variant)]
pub enum WorkspaceFeedEvent {
    Session(ServerEventEnvelope),
    Workspace(WorkspaceEventEnvelope),
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
#[allow(clippy::large_enum_variant)]
pub enum ServerEvent {
    ProjectChildWorktreeUpdated {
        worktree: ProjectWorktreeRecord,
    },
    ProjectTaskUpdated {
        task: DelegatedTaskRecord,
    },
    ProjectAgentMessageAccepted {
        message: AgentMessageRecord,
    },
    ProjectAgentCreated {
        agent: ProjectAgentRecord,
    },
    ProjectAgentUpdated {
        agent: ProjectAgentRecord,
    },
    AgentSessionCreated {
        snapshot: AgentSessionSnapshot,
    },
    AgentSessionStateChanged {
        previous: loom_core::AgentSessionState,
        current: loom_core::AgentSessionState,
    },
    AgentSessionForked {
        source_session_id: AgentSessionId,
        snapshot: AgentSessionSnapshot,
    },
    AgentSessionRenamed {
        session_id: AgentSessionId,
        name: String,
    },
    AgentSessionArchived {
        session_id: AgentSessionId,
    },
    Agent {
        event: AgentEvent,
    },
    SessionFilesystemChanged {
        change: SessionFilesystemChange,
    },
    Terminal {
        event: TerminalEventRecord,
    },
    Task {
        event: TaskEventRecord,
    },
    ProviderHealthChanged {
        provider_id: ProviderId,
        health: ProviderHealth,
    },
}

impl From<SessionEventRecord> for ServerEventEnvelope {
    fn from(record: SessionEventRecord) -> Self {
        Self::from_session_event(record.sequence, record.session_id, record.event)
    }
}

impl ServerEventEnvelope {
    pub fn from_session_event(
        sequence: EventSequence,
        session_id: AgentSessionId,
        event: SessionEvent,
    ) -> Self {
        let event = match event {
            SessionEvent::AgentSessionCreated { snapshot } => {
                ServerEvent::AgentSessionCreated { snapshot }
            }
            SessionEvent::AgentSessionStateChanged {
                previous, current, ..
            } => ServerEvent::AgentSessionStateChanged { previous, current },
            SessionEvent::AgentSessionForked {
                source_session_id,
                snapshot,
            } => ServerEvent::AgentSessionForked {
                source_session_id,
                snapshot,
            },
            SessionEvent::AgentSessionRenamed { session_id, name } => {
                ServerEvent::AgentSessionRenamed { session_id, name }
            }
            SessionEvent::AgentSessionArchived { session_id } => {
                ServerEvent::AgentSessionArchived { session_id }
            }
        };
        Self {
            protocol_version: CURRENT_PROTOCOL_VERSION,
            sequence,
            session_id,
            event,
        }
    }

    pub fn from_agent_event(
        sequence: EventSequence,
        session_id: AgentSessionId,
        event: AgentEvent,
    ) -> Self {
        Self {
            protocol_version: CURRENT_PROTOCOL_VERSION,
            sequence,
            session_id,
            event: ServerEvent::Agent { event },
        }
    }
}

#[derive(Debug, Error)]
pub enum CodecError {
    #[error("JSON codec error: {0}")]
    Json(#[from] serde_json::Error),
}

pub fn encode_request(request: &RequestEnvelope) -> Result<Vec<u8>, CodecError> {
    Ok(serde_json::to_vec(request)?)
}

pub fn decode_request(bytes: &[u8]) -> Result<RequestEnvelope, CodecError> {
    Ok(serde_json::from_slice(bytes)?)
}

pub fn encode_response(response: &ResponseEnvelope) -> Result<Vec<u8>, CodecError> {
    Ok(serde_json::to_vec(response)?)
}

pub fn decode_response(bytes: &[u8]) -> Result<ResponseEnvelope, CodecError> {
    Ok(serde_json::from_slice(bytes)?)
}

pub fn encode_event(event: &ServerEventEnvelope) -> Result<Vec<u8>, CodecError> {
    Ok(serde_json::to_vec(event)?)
}

pub fn decode_event(bytes: &[u8]) -> Result<ServerEventEnvelope, CodecError> {
    Ok(serde_json::from_slice(bytes)?)
}

pub fn encode_client_frame(frame: &ClientFrame) -> Result<Vec<u8>, CodecError> {
    Ok(serde_json::to_vec(frame)?)
}

pub fn decode_client_frame(bytes: &[u8]) -> Result<ClientFrame, CodecError> {
    Ok(serde_json::from_slice(bytes)?)
}

pub fn encode_server_frame(frame: &ServerFrame) -> Result<Vec<u8>, CodecError> {
    Ok(serde_json::to_vec(frame)?)
}

pub fn decode_server_frame(bytes: &[u8]) -> Result<ServerFrame, CodecError> {
    Ok(serde_json::from_slice(bytes)?)
}

pub fn unsupported_version_error(requested: ProtocolVersion) -> LoomError {
    LoomError::unsupported_protocol(format!(
        "protocol version {}.{} is not compatible with {}.{}",
        requested.major,
        requested.minor,
        CURRENT_PROTOCOL_VERSION.major,
        CURRENT_PROTOCOL_VERSION.minor
    ))
}

#[cfg(test)]
mod run_message_protocol_tests {
    use super::*;

    #[test]
    fn project_snapshot_request_uses_project_capability_and_round_trips() {
        assert_eq!(CURRENT_PROTOCOL_VERSION, ProtocolVersion::new(10, 0));
        let project_id = ProjectId::new();
        let request = ClientRequest::GetProjectSnapshot { project_id };
        assert_eq!(request.required_capability(), Some(Capability::ReadProject));
        let encoded = encode_request(&RequestEnvelope::new(request)).unwrap();
        assert_eq!(
            decode_request(&encoded).unwrap().protocol_version,
            CURRENT_PROTOCOL_VERSION
        );
        assert_eq!(
            decode_request(&encoded).unwrap().request,
            ClientRequest::GetProjectSnapshot { project_id }
        );

        let child_session_id = AgentSessionId::new();
        let session_request = ClientRequest::GetProjectSnapshotForSession {
            session_id: child_session_id,
        };
        assert_eq!(
            session_request.required_capability(),
            Some(Capability::ReadProject)
        );
        assert_eq!(
            decode_request(
                &encode_request(&RequestEnvelope::new(session_request.clone())).unwrap()
            )
            .unwrap()
            .request,
            session_request
        );

        let root_session_id = AgentSessionId::new();
        let snapshot = ProjectSnapshot {
            project_id,
            root_session_id,
            agents: vec![ProjectAgentRecord {
                session_id: root_session_id,
                project_id,
                parent_session_id: None,
                depth: 1,
                state: loom_core::AgentSessionState::Idle,
                task_summary: None,
                output_cursor: EventSequence::default(),
                updated_at: loom_core::Timestamp::from_unix_millis(1),
            }],
            tasks: vec![],
            worktrees: vec![],
        };
        let response = ServerResponse::ProjectSnapshot(snapshot);
        let encoded = encode_response(&ResponseEnvelope::success(
            RequestId::new(),
            response.clone(),
        ))
        .unwrap();
        assert_eq!(decode_response(&encoded).unwrap().result.unwrap(), response);
    }

    #[test]
    fn addressed_project_message_protocol_round_trip() {
        let parent = AgentSessionId::new();
        let project_id = ProjectId::new();
        let draft = AgentMessageDraft {
            project_id,
            task_id: Some(loom_core::TaskId::new()),
            sender_session_id: parent,
            target_session_id: AgentSessionId::new(),
            kind: loom_core::AgentMessageKind::Progress,
            body: "Status update".into(),
        };
        let send_request = ClientRequest::SendProjectAgentMessage {
            message: draft.clone(),
        };
        assert_eq!(
            send_request.required_capability(),
            Some(Capability::SendProjectAgentMessage)
        );
        assert!(send_request.is_retryable_mutation());
        assert_eq!(
            decode_request(&encode_request(&RequestEnvelope::new(send_request.clone())).unwrap())
                .unwrap()
                .request,
            send_request
        );

        let list_request = ClientRequest::ListProjectAgentMessages {
            project_id,
            session_id: parent,
            after_project_sequence: Some(4),
            limit: 20,
        };
        assert_eq!(
            list_request.required_capability(),
            Some(Capability::ReadProjectAgentMessages)
        );
        assert_eq!(
            decode_request(&encode_request(&RequestEnvelope::new(list_request.clone())).unwrap())
                .unwrap()
                .request,
            list_request
        );

        let control_request = ClientRequest::ControlProjectChild {
            project_id,
            manager_session_id: parent,
            task_id: loom_core::TaskId::new(),
            action: ProjectChildControlAction::Pause,
        };
        assert_eq!(
            control_request.required_capability(),
            Some(Capability::ControlProjectChild)
        );
        assert!(control_request.is_retryable_mutation());
        assert_eq!(
            decode_request(
                &encode_request(&RequestEnvelope::new(control_request.clone())).unwrap()
            )
            .unwrap()
            .request,
            control_request
        );

        let review_request = ClientRequest::GetProjectChildReview {
            project_id,
            manager_session_id: parent,
            task_id: loom_core::TaskId::new(),
        };
        assert_eq!(
            review_request.required_capability(),
            Some(Capability::ReadProjectChildReview)
        );
        assert_eq!(
            decode_request(&encode_request(&RequestEnvelope::new(review_request.clone())).unwrap())
                .unwrap()
                .request,
            review_request
        );

        let integrate_request = ClientRequest::IntegrateProjectChild {
            project_id,
            manager_session_id: parent,
            task_id: loom_core::TaskId::new(),
            expected_parent_revision: "a1b2c3".into(),
        };
        assert_eq!(
            integrate_request.required_capability(),
            Some(Capability::IntegrateProjectChild)
        );
        assert!(integrate_request.is_retryable_mutation());
        assert_eq!(
            decode_request(
                &encode_request(&RequestEnvelope::new(integrate_request.clone())).unwrap()
            )
            .unwrap()
            .request,
            integrate_request
        );

        let cleanup_request = ClientRequest::CleanupProjectChildWorktree {
            project_id,
            manager_session_id: parent,
            task_id: loom_core::TaskId::new(),
            disposition: ProjectWorktreeCleanupDisposition::Retain,
        };
        assert_eq!(
            cleanup_request.required_capability(),
            Some(Capability::CleanupProjectChildWorktree)
        );
        assert!(cleanup_request.is_retryable_mutation());
        assert_eq!(
            decode_request(
                &encode_request(&RequestEnvelope::new(cleanup_request.clone())).unwrap()
            )
            .unwrap()
            .request,
            cleanup_request
        );

        let message = AgentMessageRecord {
            message_id: loom_core::AgentMessageId::new(),
            project_id,
            task_id: draft.task_id,
            sender_session_id: draft.sender_session_id,
            target_session_id: draft.target_session_id,
            kind: draft.kind,
            project_sequence: 1,
            accepted_at: loom_core::Timestamp::from_unix_millis(10),
            body: draft.body,
        };
        let event = ServerEventEnvelope {
            protocol_version: CURRENT_PROTOCOL_VERSION,
            sequence: EventSequence::new(1),
            session_id: parent,
            event: ServerEvent::ProjectAgentMessageAccepted {
                message: message.clone(),
            },
        };
        assert_eq!(decode_event(&encode_event(&event).unwrap()).unwrap(), event);
    }

    #[test]
    fn stream_epoch_fields_default_for_sequence_only_peers() {
        let request = RequestEnvelope::new(ClientRequest::GetSessionEvents {
            session_id: Some(AgentSessionId::new()),
            workspace_id: None,
            after_sequence: Some(EventSequence::new(12)),
            stream_epoch: None,
        });
        let mut encoded = serde_json::to_value(&request).unwrap();
        encoded["request"]["data"]
            .as_object_mut()
            .unwrap()
            .remove("stream_epoch");
        encoded["request"]["data"]
            .as_object_mut()
            .unwrap()
            .remove("workspace_id");
        let decoded: RequestEnvelope = serde_json::from_value(encoded).unwrap();
        assert_eq!(decoded.request, request.request);

        let response = ResponseEnvelope::success(
            RequestId::new(),
            ServerResponse::SessionEvents {
                events: Vec::new(),
                stream_epoch: None,
            },
        );
        let mut encoded = serde_json::to_value(&response).unwrap();
        encoded["result"]["Ok"]["data"]
            .as_object_mut()
            .unwrap()
            .remove("stream_epoch");
        let decoded: ResponseEnvelope = serde_json::from_value(encoded).unwrap();
        assert_eq!(
            decoded.result.unwrap(),
            ServerResponse::SessionEvents {
                events: Vec::new(),
                stream_epoch: None,
            }
        );
    }

    #[test]
    fn child_creation_is_not_a_client_protocol_request() {
        let raw_request = serde_json::json!({
            "protocol_version": CURRENT_PROTOCOL_VERSION,
            "request_id": RequestId::new(),
            "request": {
                "type": "create_project_child",
                "data": {}
            }
        });
        assert!(decode_request(raw_request.to_string().as_bytes()).is_err());
    }

    #[test]
    fn workspace_event_snapshot_round_trips() {
        let session_id = AgentSessionId::new();
        let workspace_id = WorkspaceId::new();
        let response = ServerResponse::WorkspaceEventsSnapshot {
            workspace_id,
            sessions: Vec::new(),
            events: vec![
                WorkspaceFeedEvent::Session(ServerEventEnvelope {
                    protocol_version: CURRENT_PROTOCOL_VERSION,
                    sequence: EventSequence::new(4),
                    session_id,
                    event: ServerEvent::AgentSessionArchived { session_id },
                }),
                WorkspaceFeedEvent::Workspace(WorkspaceEventEnvelope {
                    protocol_version: CURRENT_PROTOCOL_VERSION,
                    sequence: EventSequence::new(5),
                    workspace_id,
                    event: WorkspaceEvent::Renamed {
                        name: "new name".to_owned(),
                    },
                }),
            ],
            oldest_sequence: EventSequence::new(4),
            latest_sequence: EventSequence::new(9),
            stream_epoch: Some("epoch".to_owned()),
        };
        let encoded = serde_json::to_vec(&response).unwrap();
        assert_eq!(
            serde_json::from_slice::<ServerResponse>(&encoded).unwrap(),
            response
        );
    }

    #[test]
    fn transcript_page_and_range_frames_round_trip_with_their_capability() {
        let session_id = AgentSessionId::new();
        let metadata_request = ClientRequest::GetAgentSessionSnapshotMetadata { session_id };
        assert_eq!(
            metadata_request.required_capability(),
            Some(Capability::ReadAgentSession)
        );
        let encoded = encode_request(&RequestEnvelope::new(metadata_request)).unwrap();
        assert_eq!(
            decode_request(&encoded).unwrap().request,
            ClientRequest::GetAgentSessionSnapshotMetadata { session_id }
        );

        let run_id = RunId::new();
        let page_request = ClientRequest::GetAgentRunMessagePage {
            run_id,
            before_ordinal: Some(12),
            limit: 32,
        };
        assert_eq!(
            page_request.required_capability(),
            Some(Capability::ReadAgentRunMessages)
        );
        assert_eq!(
            ClientRequest::GetAgentRunMessageContentRange {
                run_id,
                message_ordinal: 12,
                byte_offset: 0,
                length: 1,
            }
            .required_capability(),
            Some(Capability::ReadAgentRunMessages)
        );
        let encoded = encode_request(&RequestEnvelope::new(page_request)).unwrap();
        let decoded = decode_request(&encoded).unwrap();
        assert_eq!(
            decoded.request,
            ClientRequest::GetAgentRunMessagePage {
                run_id,
                before_ordinal: Some(12),
                limit: 32
            }
        );
        let transcript_request = ClientRequest::GetAgentRunTranscriptPage {
            run_id,
            before_ordinal: Some(12),
            limit: 16,
        };
        assert_eq!(
            transcript_request.required_capability(),
            Some(Capability::ReadAgentRunMessages)
        );
        assert_eq!(
            decode_request(&encode_request(&RequestEnvelope::new(transcript_request)).unwrap())
                .unwrap()
                .request,
            ClientRequest::GetAgentRunTranscriptPage {
                run_id,
                before_ordinal: Some(12),
                limit: 16,
            }
        );

        let page_response = ResponseEnvelope::success(
            RequestId::new(),
            ServerResponse::AgentRunMessagePage {
                run_id,
                messages: vec![AgentRunMessageHeader {
                    ordinal: 11,
                    role: loom_model::MessageRole::Assistant,
                    content_bytes: 18,
                    name: None,
                    tool_call_id: None,
                    tool_calls: Vec::new(),
                }],
            },
        );
        assert_eq!(
            decode_response(&encode_response(&page_response).unwrap()).unwrap(),
            page_response
        );

        let transcript_response = ResponseEnvelope::success(
            RequestId::new(),
            ServerResponse::AgentRunTranscriptPage {
                run_id,
                messages: vec![AgentRunTranscriptMessage {
                    ordinal: 11,
                    message: ModelMessage::new(loom_model::MessageRole::Assistant, "answer"),
                    content_truncated: false,
                }],
                next_before: Some(11),
                has_older: true,
            },
        );
        assert_eq!(
            decode_response(&encode_response(&transcript_response).unwrap()).unwrap(),
            transcript_response
        );

        let response = ResponseEnvelope::success(
            RequestId::new(),
            ServerResponse::AgentRunMessageContentRange {
                run_id,
                message_ordinal: 12,
                byte_offset: 256,
                content: b"bounded transcript".to_vec(),
            },
        );
        let decoded = decode_response(&encode_response(&response).unwrap()).unwrap();
        assert_eq!(decoded, response);
    }
}
