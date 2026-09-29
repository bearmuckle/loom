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

pub const CURRENT_PROTOCOL_VERSION: ProtocolVersion = ProtocolVersion::new(11, 1);
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
    /// Run-wide positions aligned with `messages`.
    #[serde(default)]
    pub message_timeline_ordinals: Vec<u64>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct AgentRunMessageHeader {
    pub ordinal: u64,
    pub timeline_ordinal: u64,
    pub role: loom_model::MessageRole,
    pub content_bytes: u64,
    pub name: Option<String>,
    pub tool_call_id: Option<loom_core::ToolCallId>,
    pub tool_calls: Vec<ToolCall>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct AgentRunTranscriptMessage {
    pub ordinal: u64,
    pub timeline_ordinal: u64,
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
pub struct ResponseEnvelope {
    pub protocol_version: ProtocolVersion,
    pub request_id: RequestId,
    pub result: std::result::Result<ServerResponse, LoomError>,
}

mod requests;
mod responses;

pub use requests::*;
pub use responses::*;

impl ClientRequest {
    pub const fn required_capability(&self) -> Option<Capability> {
        match self {
            Self::Control(ControlRequest::Negotiate { .. })
            | Self::Control(ControlRequest::DiscoverCapabilities) => None,
            Self::Control(ControlRequest::GetWorkerNodeStatus) => {
                Some(Capability::ReadWorkerNodeStatus)
            }
            Self::Workspace(WorkspaceRequest::CreateWorkspace { .. })
            | Self::Workspace(WorkspaceRequest::RegisterWorkspace { .. })
            | Self::Workspace(WorkspaceRequest::RenameWorkspace { .. }) => {
                Some(Capability::ManageWorkspaces)
            }
            Self::Workspace(WorkspaceRequest::ListWorkspaces)
            | Self::Workspace(WorkspaceRequest::ListWorkspaceSessions { .. }) => {
                Some(Capability::ReadAgentSession)
            }
            Self::Workspace(WorkspaceRequest::CreateAgentSessionInWorkspace { .. }) => {
                Some(Capability::CreateAgentSession)
            }
            Self::Workspace(WorkspaceRequest::GetWorkspaceConfigForWorkspace { .. }) => {
                Some(Capability::ReadWorkspaceConfig)
            }
            Self::Workspace(WorkspaceRequest::SetWorkspaceConfigForWorkspace { .. }) => {
                Some(Capability::ManageWorkspaces)
            }
            Self::Repository(RepositoryRequest::AttachSessionRepository { .. })
            | Self::Repository(RepositoryRequest::DetachSessionRepository { .. }) => {
                Some(Capability::ManageSessionRepositories)
            }
            Self::Filesystem(FilesystemRequest::ImportSessionDirectory { .. })
            | Self::Filesystem(FilesystemRequest::AttachSessionDirectory { .. })
            | Self::Filesystem(FilesystemRequest::DetachSessionDirectory { .. }) => {
                Some(Capability::WriteSessionFilesystem)
            }
            Self::Repository(RepositoryRequest::ListGitHubRepositories) => {
                Some(Capability::BrowseGitHubRepositories)
            }
            Self::Repository(RepositoryRequest::ListSessionRepositories { .. })
            | Self::Filesystem(FilesystemRequest::ListSessionDirectories { .. }) => {
                Some(Capability::ReadSessionFilesystem)
            }
            Self::Run(RunRequest::StartSessionAgentRun { .. })
            | Self::Run(RunRequest::StartSessionAgentRunWithOptions { .. }) => {
                Some(Capability::StartAgentRun)
            }
            Self::Filesystem(FilesystemRequest::GetSessionFilesystemSnapshot { .. })
            | Self::Filesystem(FilesystemRequest::GetSessionFilesystemChanges { .. })
            | Self::Filesystem(FilesystemRequest::ReadSessionFile { .. })
            | Self::Filesystem(FilesystemRequest::GetSessionContextFiles { .. }) => {
                Some(Capability::ReadSessionFilesystem)
            }
            Self::Filesystem(FilesystemRequest::ApplySessionFilesystemEdit { .. }) => {
                Some(Capability::WriteSessionFilesystem)
            }
            Self::Filesystem(FilesystemRequest::TakeSessionFilesystemControl { .. }) => {
                Some(Capability::WriteSessionFilesystem)
            }
            Self::Filesystem(FilesystemRequest::CreateSessionCheckpoint { .. })
            | Self::Filesystem(FilesystemRequest::RevertSessionCheckpoint { .. })
            | Self::Filesystem(FilesystemRequest::UndoSessionEdit { .. }) => {
                Some(Capability::ManageCheckpoints)
            }
            Self::Repository(RepositoryRequest::GetSessionVcsStatus { .. })
            | Self::Repository(RepositoryRequest::GetSessionVcsBranches { .. })
            | Self::Repository(RepositoryRequest::GetSessionVcsConflicts { .. }) => {
                Some(Capability::ReadVcsStatus)
            }
            Self::Repository(RepositoryRequest::GetSessionVcsDiff { .. }) => {
                Some(Capability::ReadVcsDiff)
            }
            Self::Terminal(TerminalRequest::OpenSessionTerminal { .. }) => {
                Some(Capability::OpenSessionTerminal)
            }
            Self::Terminal(TerminalRequest::WriteSessionTerminalInput { .. })
            | Self::Terminal(TerminalRequest::ResizeSessionTerminal { .. })
            | Self::Terminal(TerminalRequest::CancelSessionTerminal { .. }) => {
                Some(Capability::ControlSessionTerminal)
            }
            Self::Terminal(TerminalRequest::GetSessionTerminalEvents { .. }) => {
                Some(Capability::ControlSessionTerminal)
            }
            Self::Task(TaskRequest::StartSessionTask { .. }) => Some(Capability::StartSessionTask),
            Self::Task(TaskRequest::ListSessionTasks { .. })
            | Self::Task(TaskRequest::GetSessionTask { .. })
            | Self::Task(TaskRequest::GetSessionTaskEvents { .. }) => {
                Some(Capability::ReadSessionTask)
            }
            Self::Task(TaskRequest::CancelSessionTask { .. }) => {
                Some(Capability::ControlSessionTask)
            }
            Self::Task(TaskRequest::GetSessionTaskEvidence { .. }) => {
                Some(Capability::ReadSessionTaskEvidence)
            }
            Self::Session(SessionRequest::SetSessionApprovalPolicy { .. }) => {
                Some(Capability::ConfigureApprovalPolicy)
            }
            Self::Session(SessionRequest::GetAgentSession { .. })
            | Self::Session(SessionRequest::GetAgentSessionSnapshot { .. })
            | Self::Session(SessionRequest::GetAgentSessionSnapshotMetadata { .. })
            | Self::Session(SessionRequest::GetAgentSessionInitialState { .. }) => {
                Some(Capability::ReadAgentSession)
            }
            Self::Project(ProjectRequest::GetProjectSnapshot { .. })
            | Self::Project(ProjectRequest::GetProjectSnapshotForSession { .. }) => {
                Some(Capability::ReadProject)
            }
            Self::Project(ProjectRequest::SendProjectAgentMessage { .. }) => {
                Some(Capability::SendProjectAgentMessage)
            }
            Self::Project(ProjectRequest::ListProjectAgentMessages { .. }) => {
                Some(Capability::ReadProjectAgentMessages)
            }
            Self::Project(ProjectRequest::ControlProjectChild { .. }) => {
                Some(Capability::ControlProjectChild)
            }
            Self::Project(ProjectRequest::GetProjectChildReview { .. }) => {
                Some(Capability::ReadProjectChildReview)
            }
            Self::Project(ProjectRequest::IntegrateProjectChild { .. }) => {
                Some(Capability::IntegrateProjectChild)
            }
            Self::Project(ProjectRequest::CleanupProjectChildWorktree { .. }) => {
                Some(Capability::CleanupProjectChildWorktree)
            }
            Self::Session(SessionRequest::RenameAgentSession { .. })
            | Self::Session(SessionRequest::ArchiveAgentSession { .. }) => {
                Some(Capability::ControlAgentSession)
            }
            Self::Events(EventsRequest::GetSessionEvents { .. })
            | Self::Events(EventsRequest::GetRecentSessionEvents { .. }) => {
                Some(Capability::SubscribeSessionEvents)
            }
            Self::Run(RunRequest::GetAgentRun { .. })
            | Self::Run(RunRequest::GetAgentRunSnapshot { .. }) => Some(Capability::ReadAgentRun),
            Self::Run(RunRequest::GetAgentRunMessagePage { .. })
            | Self::Run(RunRequest::GetAgentRunTranscriptPage { .. })
            | Self::Run(RunRequest::GetAgentRunMessageContentRange { .. }) => {
                Some(Capability::ReadAgentRunMessages)
            }
            Self::Run(RunRequest::GetRunCheckpoint { .. }) => Some(Capability::ReadAgentRun),
            Self::Run(RunRequest::ApproveAgentAction { .. })
            | Self::Run(RunRequest::RejectAgentAction { .. }) => {
                Some(Capability::ApproveAgentAction)
            }
            Self::Run(RunRequest::InterruptAgentRun { .. })
            | Self::Run(RunRequest::RetryAgentStep { .. }) => Some(Capability::ControlAgentRun),
            Self::Run(RunRequest::SendAgentMessage { .. }) => Some(Capability::ControlAgentRun),
            Self::Run(RunRequest::PauseAgentRun { .. }) => Some(Capability::PauseAgentRun),
            Self::Run(RunRequest::ResumeAgentRun { .. }) => Some(Capability::ResumeAgentRun),
            Self::Run(RunRequest::RetryAgentFromCheckpoint { .. }) => {
                Some(Capability::RetryFromCheckpoint)
            }
            Self::Session(SessionRequest::ForkAgentSession { .. }) => {
                Some(Capability::ForkAgentSession)
            }
            Self::Provider(ProviderRequest::ListModels) => None,
            Self::Provider(ProviderRequest::ListProviders) => Some(Capability::ListProviders),
            Self::Provider(ProviderRequest::ConfigureGitHubCopilot { .. })
            | Self::Provider(ProviderRequest::ConfigureApiKeyProvider { .. })
            | Self::Provider(ProviderRequest::StartGitHubCopilotLogin)
            | Self::Provider(ProviderRequest::GetGitHubCopilotLoginStatus { .. }) => {
                Some(Capability::ConfigureProviders)
            }
            Self::Provider(ProviderRequest::DiscoverProviderModels { .. }) => {
                Some(Capability::ListProviders)
            }
            Self::Provider(ProviderRequest::GetProviderHealth { .. }) => {
                Some(Capability::ReadProviderHealth)
            }
            Self::Usage(UsageRequest::GetRunUsage { .. }) => Some(Capability::ReadUsage),
            Self::Usage(UsageRequest::GetSessionUsage { .. }) => Some(Capability::ReadUsage),
            Self::Context(ContextRequest::InspectAgentContext { .. }) => {
                Some(Capability::InspectContext)
            }
            Self::Run(RunRequest::AttachRunEvidence { .. }) => Some(Capability::ControlAgentRun),
        }
    }

    pub const fn is_retryable_mutation(&self) -> bool {
        matches!(
            self,
            Self::Workspace(WorkspaceRequest::CreateWorkspace { .. })
                | Self::Workspace(WorkspaceRequest::RegisterWorkspace { .. })
                | Self::Workspace(WorkspaceRequest::RenameWorkspace { .. })
                | Self::Workspace(WorkspaceRequest::CreateAgentSessionInWorkspace { .. })
                | Self::Project(ProjectRequest::ControlProjectChild { .. })
                | Self::Project(ProjectRequest::IntegrateProjectChild { .. })
                | Self::Project(ProjectRequest::CleanupProjectChildWorktree { .. })
                | Self::Workspace(WorkspaceRequest::SetWorkspaceConfigForWorkspace { .. })
                | Self::Repository(RepositoryRequest::AttachSessionRepository { .. })
                | Self::Repository(RepositoryRequest::DetachSessionRepository { .. })
                | Self::Filesystem(FilesystemRequest::AttachSessionDirectory { .. })
                | Self::Filesystem(FilesystemRequest::DetachSessionDirectory { .. })
                | Self::Run(RunRequest::StartSessionAgentRun { .. })
                | Self::Run(RunRequest::StartSessionAgentRunWithOptions { .. })
                | Self::Filesystem(FilesystemRequest::ApplySessionFilesystemEdit { .. })
                | Self::Filesystem(FilesystemRequest::TakeSessionFilesystemControl { .. })
                | Self::Filesystem(FilesystemRequest::CreateSessionCheckpoint { .. })
                | Self::Filesystem(FilesystemRequest::RevertSessionCheckpoint { .. })
                | Self::Filesystem(FilesystemRequest::UndoSessionEdit { .. })
                | Self::Terminal(TerminalRequest::OpenSessionTerminal { .. })
                | Self::Terminal(TerminalRequest::WriteSessionTerminalInput { .. })
                | Self::Terminal(TerminalRequest::ResizeSessionTerminal { .. })
                | Self::Terminal(TerminalRequest::CancelSessionTerminal { .. })
                | Self::Task(TaskRequest::StartSessionTask { .. })
                | Self::Task(TaskRequest::CancelSessionTask { .. })
                | Self::Session(SessionRequest::SetSessionApprovalPolicy { .. })
                | Self::Session(SessionRequest::RenameAgentSession { .. })
                | Self::Session(SessionRequest::ArchiveAgentSession { .. })
                | Self::Run(RunRequest::ApproveAgentAction { .. })
                | Self::Run(RunRequest::RejectAgentAction { .. })
                | Self::Run(RunRequest::SendAgentMessage { .. })
                | Self::Project(ProjectRequest::SendProjectAgentMessage { .. })
                | Self::Run(RunRequest::InterruptAgentRun { .. })
                | Self::Run(RunRequest::RetryAgentStep { .. })
                | Self::Run(RunRequest::PauseAgentRun { .. })
                | Self::Run(RunRequest::ResumeAgentRun { .. })
                | Self::Run(RunRequest::RetryAgentFromCheckpoint { .. })
                | Self::Session(SessionRequest::ForkAgentSession { .. })
                | Self::Provider(ProviderRequest::ConfigureGitHubCopilot { .. })
                | Self::Run(RunRequest::AttachRunEvidence { .. })
        )
    }
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
        assert_eq!(CURRENT_PROTOCOL_VERSION, ProtocolVersion::new(11, 1));
        let project_id = ProjectId::new();
        let request = ClientRequest::Project(ProjectRequest::GetProjectSnapshot { project_id });
        assert_eq!(request.required_capability(), Some(Capability::ReadProject));
        let encoded = encode_request(&RequestEnvelope::new(request)).unwrap();
        assert_eq!(
            decode_request(&encoded).unwrap().protocol_version,
            CURRENT_PROTOCOL_VERSION
        );
        assert_eq!(
            decode_request(&encoded).unwrap().request,
            ClientRequest::Project(ProjectRequest::GetProjectSnapshot { project_id })
        );

        let child_session_id = AgentSessionId::new();
        let session_request =
            ClientRequest::Project(ProjectRequest::GetProjectSnapshotForSession {
                session_id: child_session_id,
            });
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
        let response = ServerResponse::Project(ProjectResponse::ProjectSnapshot(snapshot));
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
        let send_request = ClientRequest::Project(ProjectRequest::SendProjectAgentMessage {
            message: draft.clone(),
        });
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

        let list_request = ClientRequest::Project(ProjectRequest::ListProjectAgentMessages {
            project_id,
            session_id: parent,
            after_project_sequence: Some(4),
            limit: 20,
        });
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

        let control_request = ClientRequest::Project(ProjectRequest::ControlProjectChild {
            project_id,
            manager_session_id: parent,
            task_id: loom_core::TaskId::new(),
            action: ProjectChildControlAction::Pause,
        });
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

        let review_request = ClientRequest::Project(ProjectRequest::GetProjectChildReview {
            project_id,
            manager_session_id: parent,
            task_id: loom_core::TaskId::new(),
        });
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

        let integrate_request = ClientRequest::Project(ProjectRequest::IntegrateProjectChild {
            project_id,
            manager_session_id: parent,
            task_id: loom_core::TaskId::new(),
            expected_parent_revision: "a1b2c3".into(),
        });
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

        let cleanup_request = ClientRequest::Project(ProjectRequest::CleanupProjectChildWorktree {
            project_id,
            manager_session_id: parent,
            task_id: loom_core::TaskId::new(),
            disposition: ProjectWorktreeCleanupDisposition::Retain,
        });
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
        let request =
            RequestEnvelope::new(ClientRequest::Events(EventsRequest::GetSessionEvents {
                session_id: Some(AgentSessionId::new()),
                workspace_id: None,
                after_sequence: Some(EventSequence::new(12)),
                stream_epoch: None,
            }));
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
            ServerResponse::Events(EventsResponse::SessionEvents {
                events: Vec::new(),
                stream_epoch: None,
            }),
        );
        let mut encoded = serde_json::to_value(&response).unwrap();
        encoded["result"]["Ok"]["data"]
            .as_object_mut()
            .unwrap()
            .remove("stream_epoch");
        let decoded: ResponseEnvelope = serde_json::from_value(encoded).unwrap();
        assert_eq!(
            decoded.result.unwrap(),
            ServerResponse::Events(EventsResponse::SessionEvents {
                events: Vec::new(),
                stream_epoch: None,
            })
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
        let response = ServerResponse::Events(EventsResponse::WorkspaceEventsSnapshot {
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
        });
        let encoded = serde_json::to_vec(&response).unwrap();
        assert_eq!(
            serde_json::from_slice::<ServerResponse>(&encoded).unwrap(),
            response
        );
    }

    #[test]
    fn transcript_page_and_range_frames_round_trip_with_their_capability() {
        let session_id = AgentSessionId::new();
        let metadata_request =
            ClientRequest::Session(SessionRequest::GetAgentSessionSnapshotMetadata { session_id });
        assert_eq!(
            metadata_request.required_capability(),
            Some(Capability::ReadAgentSession)
        );
        let encoded = encode_request(&RequestEnvelope::new(metadata_request)).unwrap();
        assert_eq!(
            decode_request(&encoded).unwrap().request,
            ClientRequest::Session(SessionRequest::GetAgentSessionSnapshotMetadata { session_id })
        );

        let run_id = RunId::new();
        let page_request = ClientRequest::Run(RunRequest::GetAgentRunMessagePage {
            run_id,
            before_ordinal: Some(12),
            limit: 32,
        });
        assert_eq!(
            page_request.required_capability(),
            Some(Capability::ReadAgentRunMessages)
        );
        assert_eq!(
            ClientRequest::Run(RunRequest::GetAgentRunMessageContentRange {
                run_id,
                message_ordinal: 12,
                byte_offset: 0,
                length: 1,
            })
            .required_capability(),
            Some(Capability::ReadAgentRunMessages)
        );
        let encoded = encode_request(&RequestEnvelope::new(page_request)).unwrap();
        let decoded = decode_request(&encoded).unwrap();
        assert_eq!(
            decoded.request,
            ClientRequest::Run(RunRequest::GetAgentRunMessagePage {
                run_id,
                before_ordinal: Some(12),
                limit: 32
            })
        );
        let transcript_request = ClientRequest::Run(RunRequest::GetAgentRunTranscriptPage {
            run_id,
            before_ordinal: Some(12),
            limit: 16,
        });
        assert_eq!(
            transcript_request.required_capability(),
            Some(Capability::ReadAgentRunMessages)
        );
        assert_eq!(
            decode_request(&encode_request(&RequestEnvelope::new(transcript_request)).unwrap())
                .unwrap()
                .request,
            ClientRequest::Run(RunRequest::GetAgentRunTranscriptPage {
                run_id,
                before_ordinal: Some(12),
                limit: 16,
            })
        );

        let page_response = ResponseEnvelope::success(
            RequestId::new(),
            ServerResponse::Run(RunResponse::AgentRunMessagePage {
                run_id,
                messages: vec![AgentRunMessageHeader {
                    ordinal: 11,
                    timeline_ordinal: 17,
                    role: loom_model::MessageRole::Assistant,
                    content_bytes: 18,
                    name: None,
                    tool_call_id: None,
                    tool_calls: Vec::new(),
                }],
            }),
        );
        assert_eq!(
            decode_response(&encode_response(&page_response).unwrap()).unwrap(),
            page_response
        );

        let transcript_response = ResponseEnvelope::success(
            RequestId::new(),
            ServerResponse::Run(RunResponse::AgentRunTranscriptPage {
                run_id,
                messages: vec![AgentRunTranscriptMessage {
                    ordinal: 11,
                    timeline_ordinal: 17,
                    message: ModelMessage::new(loom_model::MessageRole::Assistant, "answer"),
                    content_truncated: false,
                }],
                next_before: Some(11),
                has_older: true,
            }),
        );
        assert_eq!(
            decode_response(&encode_response(&transcript_response).unwrap()).unwrap(),
            transcript_response
        );

        let response = ResponseEnvelope::success(
            RequestId::new(),
            ServerResponse::Run(RunResponse::AgentRunMessageContentRange {
                run_id,
                message_ordinal: 12,
                byte_offset: 256,
                content: b"bounded transcript".to_vec(),
            }),
        );
        let decoded = decode_response(&encode_response(&response).unwrap()).unwrap();
        assert_eq!(decoded, response);
    }
}
