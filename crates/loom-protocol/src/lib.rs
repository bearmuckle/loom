use loom_core::{
    AgentSessionId, AgentSessionSnapshot, Capability, CapabilitySet, EventSequence, LoomError,
    ProjectId, ProtocolVersion, RepositoryId, RequestId, RunId, SessionEvent, SessionEventRecord,
    SessionLimits, Timestamp, ToolCallId, UsageSnapshot, WorkspaceId,
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
    AgentEvent, AgentPlan, AgentPlanStep, AgentRunSnapshot, AgentRunState, ApprovalDecision,
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
pub use vcs::{GitBranch, GitDiff, GitFileStatus, GitFileStatusKind, GitRepositoryStatus};
pub use workspace::{
    Checkpoint, CheckpointFile, ContextFileKind, ContextFileReference, RevertResult,
    SessionFilesystemChange, SessionFilesystemFile, SessionFilesystemSnapshot, SessionRepository,
    UndoResult, WorkerNodeConfig, WorkspaceChange, WorkspaceChangeKind, WorkspaceConfig,
    WorkspaceControl, WorkspaceEdit, WorkspaceEditResult, WorkspaceEntry, WorkspaceEntryKind,
    WorkspaceFile, WorkspaceRecord, WorkspaceSnapshot,
};

pub const CURRENT_PROTOCOL_VERSION: ProtocolVersion = ProtocolVersion::new(1, 2);

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ProjectSnapshot {
    pub id: ProjectId,
    pub name: String,
    pub root: Option<String>,
    pub session_count: usize,
    pub updated_at: Option<Timestamp>,
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
pub struct AgentSessionSnapshotProjection {
    pub session: AgentSessionSnapshot,
    pub active_run: Option<AgentRunSnapshotProjection>,
    pub latest_sequence: EventSequence,
    #[serde(default)]
    pub approval_policy: loom_core::ApprovalPolicy,
    #[serde(default = "default_auto_approve_actions")]
    pub auto_approve_actions: bool,
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
    ListProjects,
    ListAgentSessions {
        project_id: Option<ProjectId>,
        include_archived: bool,
    },
    CreateAgentSession {
        project_id: ProjectId,
        name: String,
    },
    GetAgentSession {
        session_id: AgentSessionId,
    },
    GetAgentSessionSnapshot {
        session_id: AgentSessionId,
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
        after_sequence: Option<EventSequence>,
    },
    GetRecentSessionEvents {
        session_id: AgentSessionId,
        limit: u32,
    },
    StartAgentRun {
        session_id: AgentSessionId,
        task: String,
        model: ModelId,
        workspace_root: String,
        system_instructions: Option<String>,
        repository_instructions: Option<String>,
    },
    StartAgentRunWithOptions {
        session_id: AgentSessionId,
        task: String,
        model: ModelId,
        workspace_root: String,
        system_instructions: Option<String>,
        repository_instructions: Option<String>,
        limits: SessionLimits,
        context: ContextAssemblyOptions,
    },
    GetAgentRun {
        run_id: RunId,
    },
    GetAgentRunSnapshot {
        run_id: RunId,
    },
    GetRunCheckpoint {
        run_id: RunId,
    },
    ApproveAgentAction {
        run_id: RunId,
        tool_call_id: ToolCallId,
    },
    RejectAgentAction {
        run_id: RunId,
        tool_call_id: ToolCallId,
        reason: Option<String>,
    },
    SendAgentMessage {
        run_id: RunId,
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
    OpenWorkspace {
        project_id: ProjectId,
        root: String,
    },
    GetWorkspaceConfig {
        project_id: ProjectId,
    },
    SetWorkspaceConfig {
        project_id: ProjectId,
        config: WorkspaceConfig,
    },
    GetWorkspaceSnapshot {
        project_id: ProjectId,
    },
    GetWorkspaceEvents {
        project_id: ProjectId,
        after_sequence: Option<EventSequence>,
    },
    GetWorkspaceChanges {
        project_id: ProjectId,
        after_sequence: Option<EventSequence>,
    },
    ReadWorkspaceFile {
        project_id: ProjectId,
        path: String,
    },
    ApplyWorkspaceEdit {
        project_id: ProjectId,
        edit: WorkspaceEdit,
    },
    TakeWorkspaceControl {
        project_id: ProjectId,
        control: WorkspaceControl,
    },
    CreateCheckpoint {
        project_id: ProjectId,
        session_id: Option<AgentSessionId>,
        label: String,
    },
    RevertCheckpoint {
        project_id: ProjectId,
        checkpoint_id: loom_core::CheckpointId,
    },
    UndoWorkspaceEdit {
        project_id: ProjectId,
    },
    SetApprovalPolicy {
        project_id: ProjectId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        session_id: Option<AgentSessionId>,
        policy: loom_core::ApprovalPolicy,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        auto_approve_actions: Option<bool>,
    },
    OpenTerminal {
        project_id: ProjectId,
        command: String,
        args: Vec<String>,
        cwd: Option<String>,
    },
    WriteTerminalInput {
        project_id: ProjectId,
        terminal_id: loom_core::TerminalId,
        input: String,
    },
    ResizeTerminal {
        project_id: ProjectId,
        terminal_id: loom_core::TerminalId,
        rows: u16,
        columns: u16,
    },
    GetTerminalEvents {
        project_id: ProjectId,
        terminal_id: loom_core::TerminalId,
        after_sequence: Option<EventSequence>,
    },
    CancelTerminal {
        project_id: ProjectId,
        terminal_id: loom_core::TerminalId,
    },
    StartTask {
        project_id: ProjectId,
        spec: TaskSpec,
    },
    ListTasks {
        project_id: ProjectId,
    },
    GetTask {
        project_id: ProjectId,
        task_id: loom_core::TaskId,
    },
    GetTaskEvents {
        project_id: ProjectId,
        task_id: loom_core::TaskId,
        after_sequence: Option<EventSequence>,
    },
    CancelTask {
        project_id: ProjectId,
        task_id: loom_core::TaskId,
    },
    GetContextFiles {
        project_id: ProjectId,
    },
    GetVcsStatus {
        project_id: ProjectId,
    },
    GetVcsDiff {
        project_id: ProjectId,
        path: Option<String>,
        staged: bool,
    },
    GetVcsBranches {
        project_id: ProjectId,
    },
    GetVcsConflicts {
        project_id: ProjectId,
    },
    GetTaskEvidence {
        project_id: ProjectId,
        task_id: loom_core::TaskId,
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
            Self::CreateWorkspace { .. } | Self::RenameWorkspace { .. } => {
                Some(Capability::ManageWorkspaces)
            }
            Self::ListWorkspaces | Self::ListWorkspaceSessions { .. } => {
                Some(Capability::ReadAgentSession)
            }
            Self::CreateAgentSessionInWorkspace { .. } => Some(Capability::CreateAgentSession),
            Self::GetWorkspaceConfigForWorkspace { .. } => Some(Capability::ReadWorkspace),
            Self::SetWorkspaceConfigForWorkspace { .. } => Some(Capability::ManageWorkspaces),
            Self::AttachSessionRepository { .. } | Self::DetachSessionRepository { .. } => {
                Some(Capability::ManageSessionRepositories)
            }
            Self::ListSessionRepositories { .. } => Some(Capability::ReadSessionFilesystem),
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
            Self::OpenSessionTerminal { .. } => Some(Capability::OpenTerminal),
            Self::WriteSessionTerminalInput { .. }
            | Self::ResizeSessionTerminal { .. }
            | Self::CancelSessionTerminal { .. } => Some(Capability::ControlTerminal),
            Self::GetSessionTerminalEvents { .. } => Some(Capability::ControlTerminal),
            Self::StartSessionTask { .. } => Some(Capability::StartTask),
            Self::ListSessionTasks { .. }
            | Self::GetSessionTask { .. }
            | Self::GetSessionTaskEvents { .. } => Some(Capability::ReadTask),
            Self::CancelSessionTask { .. } => Some(Capability::ControlTask),
            Self::GetSessionTaskEvidence { .. } => Some(Capability::ReadTaskEvidence),
            Self::SetSessionApprovalPolicy { .. } => Some(Capability::ConfigureApprovalPolicy),
            Self::ListProjects | Self::ListAgentSessions { .. } => {
                Some(Capability::ReadAgentSession)
            }
            Self::CreateAgentSession { .. } => Some(Capability::CreateAgentSession),
            Self::GetAgentSession { .. } | Self::GetAgentSessionSnapshot { .. } => {
                Some(Capability::ReadAgentSession)
            }
            Self::RenameAgentSession { .. } | Self::ArchiveAgentSession { .. } => {
                Some(Capability::ControlAgentSession)
            }
            Self::GetSessionEvents { .. } | Self::GetRecentSessionEvents { .. } => {
                Some(Capability::SubscribeSessionEvents)
            }
            Self::StartAgentRun { .. } => Some(Capability::StartAgentRun),
            Self::StartAgentRunWithOptions { .. } => Some(Capability::StartAgentRun),
            Self::GetAgentRun { .. } | Self::GetAgentRunSnapshot { .. } => {
                Some(Capability::ReadAgentRun)
            }
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
            | Self::StartGitHubCopilotLogin
            | Self::GetGitHubCopilotLoginStatus { .. } => Some(Capability::ConfigureProviders),
            Self::DiscoverProviderModels { .. } => Some(Capability::ListProviders),
            Self::GetProviderHealth { .. } => Some(Capability::ReadProviderHealth),
            Self::GetRunUsage { .. } => Some(Capability::ReadUsage),
            Self::GetSessionUsage { .. } => Some(Capability::ReadUsage),
            Self::InspectAgentContext { .. } => Some(Capability::InspectContext),
            Self::OpenWorkspace { .. } => Some(Capability::OpenWorkspace),
            Self::GetWorkspaceConfig { .. } => Some(Capability::ReadWorkspace),
            Self::SetWorkspaceConfig { .. } => Some(Capability::OpenWorkspace),
            Self::GetWorkspaceSnapshot { .. } | Self::ReadWorkspaceFile { .. } => {
                Some(Capability::ReadWorkspace)
            }
            Self::GetWorkspaceEvents { .. } => Some(Capability::SubscribeWorkspaceEvents),
            Self::GetWorkspaceChanges { .. } => Some(Capability::ReadWorkspace),
            Self::ApplyWorkspaceEdit { .. } => Some(Capability::WriteWorkspace),
            Self::TakeWorkspaceControl { .. } => Some(Capability::TakeoverWorkspace),
            Self::CreateCheckpoint { .. }
            | Self::RevertCheckpoint { .. }
            | Self::UndoWorkspaceEdit { .. } => Some(Capability::ManageCheckpoints),
            Self::SetApprovalPolicy { .. } => Some(Capability::ConfigureApprovalPolicy),
            Self::OpenTerminal { .. } => Some(Capability::OpenTerminal),
            Self::WriteTerminalInput { .. }
            | Self::ResizeTerminal { .. }
            | Self::CancelTerminal { .. } => Some(Capability::ControlTerminal),
            Self::GetTerminalEvents { .. } => Some(Capability::ControlTerminal),
            Self::StartTask { .. } => Some(Capability::StartTask),
            Self::ListTasks { .. } | Self::GetTask { .. } | Self::GetTaskEvents { .. } => {
                Some(Capability::ReadTask)
            }
            Self::CancelTask { .. } => Some(Capability::ControlTask),
            Self::GetContextFiles { .. } => Some(Capability::ReadWorkspaceInstructions),
            Self::GetVcsStatus { .. }
            | Self::GetVcsBranches { .. }
            | Self::GetVcsConflicts { .. } => Some(Capability::ReadVcsStatus),
            Self::GetVcsDiff { .. } => Some(Capability::ReadVcsDiff),
            Self::GetTaskEvidence { .. } => Some(Capability::ReadTaskEvidence),
            Self::AttachRunEvidence { .. } => Some(Capability::ControlAgentRun),
        }
    }

    pub const fn is_retryable_mutation(&self) -> bool {
        matches!(
            self,
            Self::CreateWorkspace { .. }
                | Self::RenameWorkspace { .. }
                | Self::CreateAgentSessionInWorkspace { .. }
                | Self::SetWorkspaceConfigForWorkspace { .. }
                | Self::AttachSessionRepository { .. }
                | Self::DetachSessionRepository { .. }
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
                | Self::CreateAgentSession { .. }
                | Self::RenameAgentSession { .. }
                | Self::ArchiveAgentSession { .. }
                | Self::StartAgentRun { .. }
                | Self::StartAgentRunWithOptions { .. }
                | Self::ApproveAgentAction { .. }
                | Self::RejectAgentAction { .. }
                | Self::SendAgentMessage { .. }
                | Self::InterruptAgentRun { .. }
                | Self::RetryAgentStep { .. }
                | Self::PauseAgentRun { .. }
                | Self::ResumeAgentRun { .. }
                | Self::RetryAgentFromCheckpoint { .. }
                | Self::ForkAgentSession { .. }
                | Self::ConfigureGitHubCopilot { .. }
                | Self::OpenWorkspace { .. }
                | Self::SetWorkspaceConfig { .. }
                | Self::ApplyWorkspaceEdit { .. }
                | Self::TakeWorkspaceControl { .. }
                | Self::CreateCheckpoint { .. }
                | Self::RevertCheckpoint { .. }
                | Self::UndoWorkspaceEdit { .. }
                | Self::SetApprovalPolicy { .. }
                | Self::OpenTerminal { .. }
                | Self::WriteTerminalInput { .. }
                | Self::ResizeTerminal { .. }
                | Self::CancelTerminal { .. }
                | Self::StartTask { .. }
                | Self::CancelTask { .. }
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
    Projects {
        projects: Vec<ProjectSnapshot>,
    },
    AgentSessions {
        sessions: Vec<AgentSessionSnapshot>,
    },
    AgentSessionCreated(AgentSessionSnapshot),
    AgentSessionForked(AgentSessionSnapshot),
    AgentSession(AgentSessionSnapshot),
    AgentSessionSnapshot(AgentSessionSnapshotProjection),
    AgentSessionRenamed(AgentSessionSnapshot),
    AgentSessionArchived(AgentSessionSnapshot),
    AgentRunStarted(AgentRunSnapshot),
    AgentRun(AgentRunSnapshot),
    AgentRunSnapshot(AgentRunSnapshotProjection),
    RunCheckpoint(Checkpoint),
    SessionEvents {
        events: Vec<ServerEventEnvelope>,
    },
    SessionEventsSnapshot {
        session: AgentSessionSnapshot,
        events: Vec<ServerEventEnvelope>,
        oldest_sequence: EventSequence,
        latest_sequence: EventSequence,
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
    WorkspaceOpened(WorkspaceSnapshot),
    WorkspaceConfig(WorkspaceConfig),
    WorkspaceConfigUpdated,
    WorkspaceSnapshot(WorkspaceSnapshot),
    SessionFilesystemSnapshot(SessionFilesystemSnapshot),
    WorkspaceEvents {
        events: Vec<WorkspaceChange>,
    },
    WorkspaceChanges {
        changes: Vec<WorkspaceChange>,
        truncated: bool,
    },
    SessionFilesystemChanges {
        changes: Vec<SessionFilesystemChange>,
        truncated: bool,
    },
    WorkspaceFile(WorkspaceFile),
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

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
#[allow(clippy::large_enum_variant)]
pub enum ServerEvent {
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
    WorkspaceChanged {
        change: WorkspaceChange,
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
