use loom_core::{
    AgentMessageDraft, AgentMessageRecord, AgentSessionId, AgentSessionSnapshot, Capability,
    CapabilitySet, DelegatedTaskRecord, EventSequence, LoomError, ProjectAgentRecord, ProjectId,
    ProjectSnapshot, ProjectWorktreeCleanupDisposition, ProjectWorktreeRecord, ProtocolVersion,
    RepositoryId, RequestId, RunId, SessionLimits, ToolCallId, UsageSnapshot, WorkspaceId,
};
use loom_model::{
    ModelDescriptor, ModelId, ModelMessage, ProviderHealth, ProviderId, ProviderSummary,
    ProviderUsageSummary, ToolCall,
};
use serde::{Deserialize, Serialize};
use thiserror::Error;

pub use loom_core::{
    AccessRequestDecision, AccessRequestId, CredentialId, GitHubSignInStatus,
    MAX_ACCESS_REQUEST_NOTE_BYTES, ParticipantId, ParticipantIdentity, PresenceStatus,
    ProviderCredentialMetadata, RosterEntry, RunCredentialPin, SessionAccessRequest,
    SessionCredentialSelection, SessionGrant, SessionRole,
};
pub use loom_core::{
    Author, CollaborationEntry, CollaborationEntryKind, MAX_COLLABORATION_TEXT_BYTES, Recipient,
    RunControlAction, RunRevision,
};
pub use loom_core::{
    CURRENT_PROTOCOL_VERSION, Checkpoint, CheckpointFile, ClonedRepository, ContextAssemblyOptions,
    ContextBudget, ContextFileKind, ContextFileReference, ContextInspection, ContextItem,
    ContextItemKind, ContextSummary, DEFAULT_RUN_ADMISSION_TIMEOUT_SECS,
    GITHUB_REPOSITORY_QUERY_MIN_CHARS, GitHubRepository, MAX_PROJECT_AGENT_CONCURRENCY,
    MAX_RUN_ADMISSION_TIMEOUT_SECS, MIN_PROJECT_AGENT_CONCURRENCY, MIN_RUN_ADMISSION_TIMEOUT_SECS,
    RevertResult, SessionDirectory, SessionFilesystemChange, SessionFilesystemFile,
    SessionFilesystemSnapshot, SessionRepository, TaskArtifact, TaskEvent, TaskEventRecord,
    TaskEvidenceLink, TaskKind, TaskSnapshot, TaskSpec, TaskStatus, TerminalEvent,
    TerminalEventRecord, TerminalSnapshot, TerminalStatus, TerminalStream, UndoResult,
    WorkerNodeConfig, WorkspaceChangeKind, WorkspaceConfig, WorkspaceControl, WorkspaceEdit,
    WorkspaceEditResult, WorkspaceEntry, WorkspaceEntryKind, WorkspaceRecord,
};
pub use loom_core::{
    GitBranch, GitDiff, GitDiffHunk, GitDiffLine, GitDiffLineKind, GitFileStatus,
    GitFileStatusKind, GitRepositoryStatus,
};
pub use loom_model::{
    AgentActivityData, AgentActivityKind, AgentActivityRecord, AgentActivityStatus, AgentEvent,
    AgentExecutionStateRecord, AgentInteractionKind, AgentInteractionRecord,
    AgentInteractionStatus, AgentPlan, AgentPlanStep, AgentRunAttemptRecord, AgentRunSnapshot,
    AgentRunState, AgentToolAttemptRecord, AgentToolAttemptState, AgentToolCallRecord,
    ApprovalDecision, FileActivityOperation, MAX_AGENT_RUN_MESSAGE_CONTENT_RANGE_BYTES,
    MAX_AGENT_RUN_MESSAGE_PAGE_SIZE, MAX_AGENT_RUN_TRANSCRIPT_MESSAGE_BYTES,
    MAX_AGENT_RUN_TRANSCRIPT_PAGE_SIZE, ProjectJoinContinuation, ServerEvent, ServerEventEnvelope,
    ToolResult, ToolResultKind, WorkspaceEvent, WorkspaceEventEnvelope, WorkspaceFeedEvent,
    tool_result_kind,
};

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProjectChildControlAction {
    Continue,
    RetryFailedStep,
    Pause,
    Interrupt,
    Cancel,
    /// Closes a finished child task as failed without landing its result.
    ///
    /// A finished code child that left uncommitted work in its checkout blocks
    /// the manager's completion guard with no integration available; abandoning
    /// records the uncommitted paths, marks the task failed, and releases the
    /// guard while preserving the checkout.
    Abandon,
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

/// Active backend archive-retention policy, exposed read-only to clients.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ArchiveRetentionPolicy {
    /// Retention window in milliseconds; `None` or zero means retention is disabled.
    #[serde(default)]
    pub retention_ms: Option<u64>,
    /// Whether an automatic sweep may discard dirty or locked worktrees.
    #[serde(default)]
    pub force_discard_worktrees: bool,
    /// Window in milliseconds after which an archived sub-agent session is
    /// deleted on its own; `None` or zero disables it. Absent in older payloads,
    /// which then get the 10 minute default.
    #[serde(default = "default_sub_agent_retention_ms")]
    pub sub_agent_retention_ms: Option<u64>,
}

/// Default archived sub-agent retention: 10 minutes.
pub const DEFAULT_SUB_AGENT_RETENTION_MS: u64 = 600_000;

fn default_sub_agent_retention_ms() -> Option<u64> {
    Some(DEFAULT_SUB_AGENT_RETENTION_MS)
}

impl Default for ArchiveRetentionPolicy {
    fn default() -> Self {
        Self::disabled()
    }
}

impl ArchiveRetentionPolicy {
    /// Policy used when general retention is turned off. The sub-agent window
    /// keeps its default.
    pub const fn disabled() -> Self {
        Self {
            retention_ms: None,
            force_discard_worktrees: false,
            sub_agent_retention_ms: Some(DEFAULT_SUB_AGENT_RETENTION_MS),
        }
    }

    /// Sub-agent retention window, or `None` when it is disabled.
    pub fn sub_agent_retention(&self) -> Option<std::time::Duration> {
        self.sub_agent_retention_ms
            .filter(|ms| *ms > 0)
            .map(std::time::Duration::from_millis)
    }

    /// Whether the policy keeps archived sessions alive for a retention window.
    pub fn is_enabled(&self) -> bool {
        self.retention_ms
            .is_some_and(|retention_ms| retention_ms > 0)
    }

    /// Retention window, or `None` when retention is disabled.
    pub fn retention(&self) -> Option<std::time::Duration> {
        self.retention_ms
            .filter(|retention_ms| *retention_ms > 0)
            .map(std::time::Duration::from_millis)
    }
}

#[cfg(test)]
mod archive_retention_policy_tests {
    use super::ArchiveRetentionPolicy;

    #[test]
    fn disabled_policy_reports_no_retention_window() {
        let policy = ArchiveRetentionPolicy::disabled();
        assert_eq!(policy, ArchiveRetentionPolicy::default());
        assert!(!policy.is_enabled());
        assert_eq!(policy.retention(), None);
        assert_eq!(
            policy.sub_agent_retention(),
            Some(std::time::Duration::from_secs(600))
        );
    }

    #[test]
    fn old_payload_defaults_sub_agent_window_to_ten_minutes() {
        let policy: ArchiveRetentionPolicy = serde_json::from_str(r#"{"retention_ms":5}"#).unwrap();
        assert_eq!(policy.sub_agent_retention_ms, Some(600_000));
        let off: ArchiveRetentionPolicy =
            serde_json::from_str(r#"{"sub_agent_retention_ms":null}"#).unwrap();
        assert_eq!(off.sub_agent_retention_ms, None);
    }

    #[test]
    fn positive_retention_window_enables_the_policy() {
        let policy = ArchiveRetentionPolicy {
            retention_ms: Some(1_500),
            force_discard_worktrees: true,
            sub_agent_retention_ms: Some(0),
        };
        assert_eq!(policy.sub_agent_retention(), None);
        assert!(policy.is_enabled());
        assert_eq!(
            policy.retention(),
            Some(std::time::Duration::from_millis(1_500))
        );
    }

    #[test]
    fn zero_retention_window_is_disabled() {
        let policy = ArchiveRetentionPolicy {
            retention_ms: Some(0),
            force_discard_worktrees: true,
            sub_agent_retention_ms: None,
        };
        assert!(!policy.is_enabled());
        assert_eq!(policy.retention(), None);
    }
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

/// Plan progress derived from a run's step events.
///
/// Plan steps advance with the agent's model steps, so a reconnecting client
/// can restore exactly the completed and active markers it showed live rather
/// than resetting the plan to "not started".
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct AgentPlanProgress {
    /// Step indices reported complete, in ascending order.
    #[serde(default)]
    pub completed: Vec<u32>,
    /// The step currently executing, if a step is in flight.
    #[serde(default)]
    pub active: Option<u32>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct AgentRunSnapshotProjection {
    pub run: AgentRunSnapshot,
    pub plan: Vec<AgentPlanStep>,
    /// Progress for `plan`, or an empty default for peers that predate it.
    #[serde(default)]
    pub plan_progress: AgentPlanProgress,
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
    /// Provider reasoning kept for display after a reload.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_content: Option<String>,
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
    /// `true` only when the worker answered a repeated request id from its
    /// idempotency cache instead of performing the operation again.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub replayed: bool,
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
            Self::Control(ControlRequest::GetArchiveRetentionPolicy) => {
                Some(Capability::ReadWorkerNodeStatus)
            }
            Self::Control(ControlRequest::SetArchiveRetentionPolicy { .. }) => {
                Some(Capability::ConfigureArchiveRetention)
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
            Self::Repository(RepositoryRequest::SearchGitHubRepositories { .. })
            | Self::Repository(RepositoryRequest::ListClonedRepositories) => {
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
            // A sub project is an agent session of its own, so creating one
            // needs the capability that creates sessions.
            Self::Project(ProjectRequest::CreateSubProject { .. }) => {
                Some(Capability::CreateAgentSession)
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
            | Self::Session(SessionRequest::SetAgentSessionGoal { .. })
            | Self::Session(SessionRequest::SetAgentSessionTitleSource { .. })
            | Self::Session(SessionRequest::ArchiveAgentSession { .. })
            | Self::Session(SessionRequest::RestoreAgentSession { .. }) => {
                Some(Capability::ControlAgentSession)
            }
            Self::Session(SessionRequest::DeleteAgentSession { .. }) => {
                Some(Capability::DeleteAgentSession)
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
            Self::Run(RunRequest::SendAgentMessage { .. })
            | Self::Run(RunRequest::SetAgentRunModel { .. }) => Some(Capability::ControlAgentRun),
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
            | Self::Provider(ProviderRequest::RemoveApiKeyProvider { .. })
            | Self::Provider(ProviderRequest::StartGitHubCopilotLogin)
            | Self::Provider(ProviderRequest::GetGitHubCopilotLoginStatus { .. })
            | Self::Provider(ProviderRequest::ConfigureGitHubRepository { .. })
            | Self::Provider(ProviderRequest::StartGitHubRepositoryLogin)
            | Self::Provider(ProviderRequest::GetGitHubRepositoryLoginStatus { .. })
            | Self::Provider(ProviderRequest::ConfigureGitHubWriteAccess { .. }) => {
                Some(Capability::ConfigureProviders)
            }
            Self::Provider(ProviderRequest::GetGitHubWriteAccess) => {
                Some(Capability::ListProviders)
            }
            Self::Provider(ProviderRequest::GetGitHubRepositoryAccess) => {
                Some(Capability::ListProviders)
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
            Self::Access(AccessRequest::GetCurrentParticipant) => None,
            Self::Access(AccessRequest::ListParticipants)
            | Self::Access(AccessRequest::GrantSessionAccess { .. })
            | Self::Access(AccessRequest::SetSessionRole { .. })
            | Self::Access(AccessRequest::RevokeSessionAccess { .. }) => {
                Some(Capability::ManageSessionAccess)
            }
            Self::Access(AccessRequest::GetSessionRoster { .. }) => {
                Some(Capability::ReadSessionRoster)
            }
            Self::Collaboration(CollaborationRequest::SendPeerMessage { .. }) => {
                Some(Capability::SendPeerMessage)
            }
            Self::Collaboration(CollaborationRequest::DirectAgent { .. })
            | Self::Collaboration(CollaborationRequest::AnswerAgentQuestion { .. }) => {
                Some(Capability::ControlAgentRun)
            }
            Self::Collaboration(CollaborationRequest::ControlRun { action, .. }) => {
                Some(match action {
                    RunControlAction::Approve { .. } | RunControlAction::Reject { .. } => {
                        Capability::ApproveAgentAction
                    }
                    RunControlAction::Pause => Capability::PauseAgentRun,
                    RunControlAction::Resume => Capability::ResumeAgentRun,
                    RunControlAction::Interrupt | RunControlAction::Retry => {
                        Capability::ControlAgentRun
                    }
                    RunControlAction::RetryFromCheckpoint { .. } => Capability::RetryFromCheckpoint,
                })
            }
            Self::Collaboration(CollaborationRequest::GetRunRevision { .. }) => {
                Some(Capability::ReadAgentRun)
            }
            Self::Collaboration(CollaborationRequest::ListSessionCollaboration { .. }) => {
                Some(Capability::ReadAgentSession)
            }
            Self::Access(AccessRequest::ListProviderCredentials)
            | Self::Access(AccessRequest::GetSessionCredential { .. })
            | Self::Access(AccessRequest::GetRunCredential { .. })
            | Self::Access(AccessRequest::RevokeProviderCredential { .. }) => {
                Some(Capability::ListProviders)
            }
            Self::Access(AccessRequest::SelectSessionCredential { .. }) => {
                Some(Capability::ControlAgentSession)
            }
            Self::Access(AccessRequest::RequestSessionAccess { .. })
            | Self::Access(AccessRequest::WithdrawSessionAccessRequest { .. }) => {
                Some(Capability::RequestSessionAccess)
            }
            Self::Access(AccessRequest::ListSessionAccessRequests { .. })
            | Self::Access(AccessRequest::ApproveSessionAccessRequest { .. })
            | Self::Access(AccessRequest::DenySessionAccessRequest { .. }) => {
                Some(Capability::ManageSessionAccess)
            }
            Self::Access(AccessRequest::StartGitHubSignIn)
            | Self::Access(AccessRequest::ResumeGitHubIdentity { .. }) => {
                Some(Capability::GitHubSignIn)
            }
            Self::Access(AccessRequest::GetGitHubSignInStatus { .. })
            | Self::Access(AccessRequest::SignOutGitHub) => None,
        }
    }

    /// The least session role a person needs for this request, when the
    /// request reads or acts on one session (or a run, project, or child of
    /// it).
    ///
    /// `None` means the request is not scoped to a session: it is governed by
    /// the connection's token scope alone, and listings it returns are
    /// filtered to the sessions the person may read. An operator connection
    /// that is not bound to a person is not subject to roles.
    pub const fn required_session_role(&self) -> Option<SessionRole> {
        use SessionRole::{Observer, Owner, Participant};
        match self {
            Self::Control(_)
            | Self::Provider(_)
            | Self::Workspace(_)
            | Self::Repository(RepositoryRequest::SearchGitHubRepositories {
                session_id: None,
                ..
            })
            | Self::Repository(RepositoryRequest::ListClonedRepositories)
            | Self::Access(AccessRequest::GetCurrentParticipant)
            | Self::Access(AccessRequest::ResumeGitHubIdentity { .. })
            | Self::Access(AccessRequest::ListParticipants)
            | Self::Access(AccessRequest::ListProviderCredentials)
            | Self::Access(AccessRequest::RevokeProviderCredential { .. })
            // Asking for access needs only a bound identity; withdrawing is
            // limited to the requester by the backend.
            | Self::Access(AccessRequest::RequestSessionAccess { .. })
            | Self::Access(AccessRequest::WithdrawSessionAccessRequest { .. }) => None,
            // Signing in, polling a sign-in, and signing out need no grant: a
            // pre-login connection has no identity to hold one yet.
            Self::Access(AccessRequest::StartGitHubSignIn)
            | Self::Access(AccessRequest::GetGitHubSignInStatus { .. })
            | Self::Access(AccessRequest::SignOutGitHub) => None,
            Self::Events(EventsRequest::GetSessionEvents { session_id, .. }) => {
                if session_id.is_some() {
                    Some(Observer)
                } else {
                    None
                }
            }
            Self::Events(EventsRequest::GetRecentSessionEvents { .. })
            | Self::Session(SessionRequest::GetAgentSession { .. })
            | Self::Session(SessionRequest::GetAgentSessionSnapshot { .. })
            | Self::Session(SessionRequest::GetAgentSessionSnapshotMetadata { .. })
            | Self::Session(SessionRequest::GetAgentSessionInitialState { .. })
            | Self::Run(RunRequest::GetAgentRun { .. })
            | Self::Run(RunRequest::GetAgentRunMessagePage { .. })
            | Self::Run(RunRequest::GetAgentRunTranscriptPage { .. })
            | Self::Run(RunRequest::GetAgentRunMessageContentRange { .. })
            | Self::Run(RunRequest::GetAgentRunSnapshot { .. })
            | Self::Run(RunRequest::GetRunCheckpoint { .. })
            | Self::Usage(_)
            | Self::Context(_)
            | Self::Repository(RepositoryRequest::ListSessionRepositories { .. })
            | Self::Repository(RepositoryRequest::GetSessionVcsStatus { .. })
            | Self::Repository(RepositoryRequest::GetSessionVcsDiff { .. })
            | Self::Repository(RepositoryRequest::GetSessionVcsBranches { .. })
            | Self::Repository(RepositoryRequest::GetSessionVcsConflicts { .. })
            | Self::Filesystem(FilesystemRequest::ListSessionDirectories { .. })
            | Self::Filesystem(FilesystemRequest::GetSessionFilesystemSnapshot { .. })
            | Self::Filesystem(FilesystemRequest::GetSessionFilesystemChanges { .. })
            | Self::Filesystem(FilesystemRequest::ReadSessionFile { .. })
            | Self::Filesystem(FilesystemRequest::GetSessionContextFiles { .. })
            | Self::Terminal(TerminalRequest::GetSessionTerminalEvents { .. })
            | Self::Task(TaskRequest::ListSessionTasks { .. })
            | Self::Task(TaskRequest::GetSessionTask { .. })
            | Self::Task(TaskRequest::GetSessionTaskEvents { .. })
            | Self::Task(TaskRequest::GetSessionTaskEvidence { .. })
            | Self::Project(ProjectRequest::GetProjectSnapshot { .. })
            | Self::Project(ProjectRequest::GetProjectSnapshotForSession { .. })
            | Self::Project(ProjectRequest::ListProjectAgentMessages { .. })
            | Self::Project(ProjectRequest::GetProjectChildReview { .. })
            | Self::Collaboration(CollaborationRequest::GetRunRevision { .. })
            | Self::Collaboration(CollaborationRequest::ListSessionCollaboration { .. })
            | Self::Access(AccessRequest::GetSessionCredential { .. })
            | Self::Access(AccessRequest::GetRunCredential { .. })
            | Self::Access(AccessRequest::GetSessionRoster { .. }) => Some(Observer),
            Self::Run(RunRequest::StartSessionAgentRun { .. })
            | Self::Run(RunRequest::StartSessionAgentRunWithOptions { .. })
            | Self::Run(RunRequest::ApproveAgentAction { .. })
            | Self::Run(RunRequest::RejectAgentAction { .. })
            | Self::Run(RunRequest::SendAgentMessage { .. })
            | Self::Run(RunRequest::SetAgentRunModel { .. })
            | Self::Run(RunRequest::InterruptAgentRun { .. })
            | Self::Run(RunRequest::RetryAgentStep { .. })
            | Self::Run(RunRequest::PauseAgentRun { .. })
            | Self::Run(RunRequest::ResumeAgentRun { .. })
            | Self::Run(RunRequest::RetryAgentFromCheckpoint { .. })
            | Self::Run(RunRequest::AttachRunEvidence { .. })
            | Self::Session(SessionRequest::ForkAgentSession { .. })
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
            | Self::Project(ProjectRequest::SendProjectAgentMessage { .. })
            | Self::Project(ProjectRequest::ControlProjectChild { .. })
            | Self::Project(ProjectRequest::IntegrateProjectChild { .. })
            | Self::Project(ProjectRequest::CleanupProjectChildWorktree { .. })
            | Self::Collaboration(CollaborationRequest::SendPeerMessage { .. })
            | Self::Collaboration(CollaborationRequest::DirectAgent { .. })
            | Self::Collaboration(CollaborationRequest::AnswerAgentQuestion { .. })
            | Self::Collaboration(CollaborationRequest::ControlRun { .. }) => Some(Participant),
            Self::Session(SessionRequest::RenameAgentSession { .. })
            | Self::Session(SessionRequest::SetAgentSessionGoal { .. })
            | Self::Session(SessionRequest::SetAgentSessionTitleSource { .. })
            | Self::Session(SessionRequest::ArchiveAgentSession { .. })
            | Self::Session(SessionRequest::RestoreAgentSession { .. })
            | Self::Session(SessionRequest::DeleteAgentSession { .. })
            | Self::Session(SessionRequest::SetSessionApprovalPolicy { .. })
            | Self::Repository(RepositoryRequest::AttachSessionRepository { .. })
            | Self::Repository(RepositoryRequest::DetachSessionRepository { .. })
            | Self::Filesystem(FilesystemRequest::ImportSessionDirectory { .. })
            | Self::Filesystem(FilesystemRequest::AttachSessionDirectory { .. })
            | Self::Filesystem(FilesystemRequest::DetachSessionDirectory { .. })
            | Self::Access(AccessRequest::GrantSessionAccess { .. })
            | Self::Access(AccessRequest::SetSessionRole { .. })
            | Self::Access(AccessRequest::SelectSessionCredential { .. })
            | Self::Repository(RepositoryRequest::SearchGitHubRepositories {
                session_id: Some(_),
                ..
            })
            | Self::Access(AccessRequest::ListSessionAccessRequests { .. })
            | Self::Access(AccessRequest::ApproveSessionAccessRequest { .. })
            | Self::Access(AccessRequest::DenySessionAccessRequest { .. })
            | Self::Access(AccessRequest::RevokeSessionAccess { .. }) => Some(Owner),
            // Creating a sub project adds a project next to the one it is
            // branched out of, so it needs the role that may change that
            // project.
            Self::Project(ProjectRequest::CreateSubProject { .. }) => Some(Owner),
        }
    }

    /// What a session-scoped request reads or acts on, so a backend can find
    /// the session whose role governs it. `None` when
    /// [`Self::required_session_role`] is `None`.
    pub const fn access_target(&self) -> Option<AccessTarget> {
        use AccessTarget::{Project, Run, Session};
        match self {
            Self::Events(EventsRequest::GetSessionEvents {
                session_id: Some(session_id),
                ..
            })
            | Self::Events(EventsRequest::GetRecentSessionEvents { session_id, .. })
            | Self::Session(SessionRequest::GetAgentSession { session_id })
            | Self::Session(SessionRequest::GetAgentSessionSnapshot { session_id, .. })
            | Self::Session(SessionRequest::GetAgentSessionSnapshotMetadata {
                session_id, ..
            })
            | Self::Session(SessionRequest::GetAgentSessionInitialState { session_id, .. })
            | Self::Session(SessionRequest::RenameAgentSession { session_id, .. })
            | Self::Session(SessionRequest::SetAgentSessionGoal { session_id, .. })
            | Self::Session(SessionRequest::SetAgentSessionTitleSource { session_id, .. })
            | Self::Session(SessionRequest::ArchiveAgentSession { session_id })
            | Self::Session(SessionRequest::RestoreAgentSession { session_id })
            | Self::Session(SessionRequest::DeleteAgentSession { session_id, .. })
            | Self::Session(SessionRequest::ForkAgentSession { session_id, .. })
            | Self::Session(SessionRequest::SetSessionApprovalPolicy { session_id, .. })
            | Self::Usage(UsageRequest::GetSessionUsage { session_id })
            | Self::Repository(RepositoryRequest::AttachSessionRepository { session_id, .. })
            | Self::Repository(RepositoryRequest::ListSessionRepositories { session_id })
            | Self::Repository(RepositoryRequest::DetachSessionRepository { session_id, .. })
            | Self::Repository(RepositoryRequest::GetSessionVcsStatus { session_id, .. })
            | Self::Repository(RepositoryRequest::GetSessionVcsDiff { session_id, .. })
            | Self::Repository(RepositoryRequest::GetSessionVcsBranches { session_id, .. })
            | Self::Repository(RepositoryRequest::GetSessionVcsConflicts { session_id, .. })
            | Self::Filesystem(FilesystemRequest::ImportSessionDirectory { session_id, .. })
            | Self::Filesystem(FilesystemRequest::AttachSessionDirectory { session_id, .. })
            | Self::Filesystem(FilesystemRequest::ListSessionDirectories { session_id })
            | Self::Filesystem(FilesystemRequest::DetachSessionDirectory { session_id, .. })
            | Self::Filesystem(FilesystemRequest::GetSessionFilesystemSnapshot { session_id })
            | Self::Filesystem(FilesystemRequest::GetSessionFilesystemChanges {
                session_id, ..
            })
            | Self::Filesystem(FilesystemRequest::ReadSessionFile { session_id, .. })
            | Self::Filesystem(FilesystemRequest::ApplySessionFilesystemEdit {
                session_id, ..
            })
            | Self::Filesystem(FilesystemRequest::TakeSessionFilesystemControl {
                session_id,
                ..
            })
            | Self::Filesystem(FilesystemRequest::CreateSessionCheckpoint { session_id, .. })
            | Self::Filesystem(FilesystemRequest::RevertSessionCheckpoint { session_id, .. })
            | Self::Filesystem(FilesystemRequest::UndoSessionEdit { session_id })
            | Self::Filesystem(FilesystemRequest::GetSessionContextFiles { session_id })
            | Self::Terminal(TerminalRequest::OpenSessionTerminal { session_id, .. })
            | Self::Terminal(TerminalRequest::WriteSessionTerminalInput { session_id, .. })
            | Self::Terminal(TerminalRequest::ResizeSessionTerminal { session_id, .. })
            | Self::Terminal(TerminalRequest::GetSessionTerminalEvents { session_id, .. })
            | Self::Terminal(TerminalRequest::CancelSessionTerminal { session_id, .. })
            | Self::Task(TaskRequest::StartSessionTask { session_id, .. })
            | Self::Task(TaskRequest::ListSessionTasks { session_id })
            | Self::Task(TaskRequest::GetSessionTask { session_id, .. })
            | Self::Task(TaskRequest::GetSessionTaskEvents { session_id, .. })
            | Self::Task(TaskRequest::CancelSessionTask { session_id, .. })
            | Self::Task(TaskRequest::GetSessionTaskEvidence { session_id, .. })
            | Self::Run(RunRequest::StartSessionAgentRun { session_id, .. })
            | Self::Run(RunRequest::StartSessionAgentRunWithOptions { session_id, .. })
            | Self::Project(ProjectRequest::GetProjectSnapshotForSession { session_id })
            | Self::Project(ProjectRequest::ListProjectAgentMessages { session_id, .. })
            | Self::Project(ProjectRequest::ControlProjectChild {
                manager_session_id: session_id,
                ..
            })
            | Self::Project(ProjectRequest::GetProjectChildReview {
                manager_session_id: session_id,
                ..
            })
            | Self::Project(ProjectRequest::IntegrateProjectChild {
                manager_session_id: session_id,
                ..
            })
            | Self::Project(ProjectRequest::CleanupProjectChildWorktree {
                manager_session_id: session_id,
                ..
            })
            | Self::Access(AccessRequest::GetSessionRoster { session_id })
            | Self::Access(AccessRequest::GrantSessionAccess { session_id, .. })
            | Self::Access(AccessRequest::SetSessionRole { session_id, .. })
            | Self::Access(AccessRequest::SelectSessionCredential { session_id, .. })
            | Self::Access(AccessRequest::GetSessionCredential { session_id })
            | Self::Repository(RepositoryRequest::SearchGitHubRepositories {
                session_id: Some(session_id),
                ..
            })
            | Self::Access(AccessRequest::RevokeSessionAccess { session_id, .. })
            | Self::Access(AccessRequest::ListSessionAccessRequests { session_id })
            | Self::Access(AccessRequest::ApproveSessionAccessRequest { session_id, .. })
            | Self::Access(AccessRequest::DenySessionAccessRequest { session_id, .. })
            | Self::Collaboration(CollaborationRequest::SendPeerMessage { session_id, .. })
            | Self::Collaboration(CollaborationRequest::ListSessionCollaboration {
                session_id,
                ..
            }) => Some(Session(*session_id)),
            // Creating a sub project acts on the project it branches out of.
            Self::Project(ProjectRequest::CreateSubProject {
                parent_session_id, ..
            }) => Some(Session(*parent_session_id)),
            Self::Access(AccessRequest::GetRunCredential { run_id })
            | Self::Run(RunRequest::GetAgentRun { run_id })
            | Self::Run(RunRequest::SetAgentRunModel { run_id, .. })
            | Self::Run(RunRequest::GetAgentRunMessagePage { run_id, .. })
            | Self::Run(RunRequest::GetAgentRunTranscriptPage { run_id, .. })
            | Self::Run(RunRequest::GetAgentRunMessageContentRange { run_id, .. })
            | Self::Run(RunRequest::GetAgentRunSnapshot { run_id, .. })
            | Self::Run(RunRequest::GetRunCheckpoint { run_id })
            | Self::Run(RunRequest::ApproveAgentAction { run_id, .. })
            | Self::Run(RunRequest::RejectAgentAction { run_id, .. })
            | Self::Run(RunRequest::SendAgentMessage { run_id, .. })
            | Self::Run(RunRequest::InterruptAgentRun { run_id })
            | Self::Run(RunRequest::RetryAgentStep { run_id })
            | Self::Run(RunRequest::PauseAgentRun { run_id })
            | Self::Run(RunRequest::ResumeAgentRun { run_id })
            | Self::Run(RunRequest::RetryAgentFromCheckpoint { run_id, .. })
            | Self::Run(RunRequest::AttachRunEvidence { run_id, .. })
            | Self::Usage(UsageRequest::GetRunUsage { run_id })
            | Self::Collaboration(CollaborationRequest::DirectAgent { run_id, .. })
            | Self::Collaboration(CollaborationRequest::AnswerAgentQuestion { run_id, .. })
            | Self::Collaboration(CollaborationRequest::ControlRun { run_id, .. })
            | Self::Collaboration(CollaborationRequest::GetRunRevision { run_id })
            | Self::Context(ContextRequest::InspectAgentContext { run_id }) => Some(Run(*run_id)),
            Self::Project(ProjectRequest::GetProjectSnapshot { project_id }) => {
                Some(Project(*project_id))
            }
            Self::Project(ProjectRequest::SendProjectAgentMessage { message }) => {
                Some(Project(message.project_id))
            }
            _ => None,
        }
    }

    pub const fn is_retryable_mutation(&self) -> bool {
        matches!(
            self,
            Self::Workspace(WorkspaceRequest::CreateWorkspace { .. })
                | Self::Workspace(WorkspaceRequest::RegisterWorkspace { .. })
                | Self::Workspace(WorkspaceRequest::RenameWorkspace { .. })
                | Self::Workspace(WorkspaceRequest::CreateAgentSessionInWorkspace { .. })
                | Self::Project(ProjectRequest::CreateSubProject { .. })
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
                | Self::Session(SessionRequest::SetAgentSessionGoal { .. })
                | Self::Session(SessionRequest::SetAgentSessionTitleSource { .. })
                | Self::Session(SessionRequest::ArchiveAgentSession { .. })
                | Self::Session(SessionRequest::RestoreAgentSession { .. })
                | Self::Session(SessionRequest::DeleteAgentSession { .. })
                | Self::Control(ControlRequest::SetArchiveRetentionPolicy { .. })
                | Self::Run(RunRequest::ApproveAgentAction { .. })
                | Self::Run(RunRequest::RejectAgentAction { .. })
                | Self::Run(RunRequest::SendAgentMessage { .. })
                | Self::Run(RunRequest::SetAgentRunModel { .. })
                | Self::Project(ProjectRequest::SendProjectAgentMessage { .. })
                | Self::Run(RunRequest::InterruptAgentRun { .. })
                | Self::Run(RunRequest::RetryAgentStep { .. })
                | Self::Run(RunRequest::PauseAgentRun { .. })
                | Self::Run(RunRequest::ResumeAgentRun { .. })
                | Self::Run(RunRequest::RetryAgentFromCheckpoint { .. })
                | Self::Session(SessionRequest::ForkAgentSession { .. })
                | Self::Provider(ProviderRequest::ConfigureGitHubWriteAccess { .. })
                | Self::Run(RunRequest::AttachRunEvidence { .. })
                | Self::Access(AccessRequest::GrantSessionAccess { .. })
                | Self::Access(AccessRequest::SetSessionRole { .. })
                | Self::Access(AccessRequest::RevokeSessionAccess { .. })
                | Self::Collaboration(CollaborationRequest::SendPeerMessage { .. })
                | Self::Collaboration(CollaborationRequest::DirectAgent { .. })
                | Self::Collaboration(CollaborationRequest::AnswerAgentQuestion { .. })
                | Self::Collaboration(CollaborationRequest::ControlRun { .. })
                | Self::Access(AccessRequest::SelectSessionCredential { .. })
                | Self::Access(AccessRequest::RevokeProviderCredential { .. })
                | Self::Access(AccessRequest::RequestSessionAccess { .. })
                | Self::Access(AccessRequest::ApproveSessionAccessRequest { .. })
                | Self::Access(AccessRequest::DenySessionAccessRequest { .. })
                | Self::Access(AccessRequest::WithdrawSessionAccessRequest { .. })
        )
    }
}

impl ResponseEnvelope {
    pub fn success(request_id: RequestId, response: ServerResponse) -> Self {
        Self {
            protocol_version: CURRENT_PROTOCOL_VERSION,
            request_id,
            result: Ok(response),
            replayed: false,
        }
    }

    pub fn failure(request_id: RequestId, error: LoomError) -> Self {
        Self {
            protocol_version: CURRENT_PROTOCOL_VERSION,
            request_id,
            result: Err(error),
            replayed: false,
        }
    }

    /// Marks an idempotent replay: the answer is served from the idempotency
    /// cache rather than by running the operation again. The flag is set on the
    /// copy handed to the caller and is never stored back into the cache.
    pub fn into_replay(mut self) -> Self {
        self.replayed = true;
        self
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum GitHubCopilotLoginStatus {
    Pending,
    Configured,
    Failed { message: String },
}

/// The entity a session-scoped request names; see
/// [`ClientRequest::access_target`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AccessTarget {
    Session(AgentSessionId),
    Run(RunId),
    /// A project is named by its root session.
    Project(ProjectId),
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
    fn access_requests_carry_roles_targets_and_round_trip() {
        let session_id = AgentSessionId::new();
        let participant_id = ParticipantId::new();
        let manage = ClientRequest::Access(AccessRequest::SetSessionRole {
            session_id,
            participant_id,
            role: SessionRole::Participant,
        });
        assert_eq!(
            manage.required_capability(),
            Some(Capability::ManageSessionAccess)
        );
        assert_eq!(manage.required_session_role(), Some(SessionRole::Owner));
        assert_eq!(
            manage.access_target(),
            Some(AccessTarget::Session(session_id))
        );
        assert!(manage.is_retryable_mutation());
        let encoded = serde_json::to_value(&manage).unwrap();
        assert_eq!(encoded["type"], "access");
        assert_eq!(encoded["data"]["type"], "set_session_role");
        assert_eq!(encoded["data"]["data"]["role"], "participant");
        assert_eq!(
            serde_json::from_value::<ClientRequest>(encoded).unwrap(),
            manage
        );

        let roster = ClientRequest::Access(AccessRequest::GetSessionRoster { session_id });
        assert_eq!(
            roster.required_capability(),
            Some(Capability::ReadSessionRoster)
        );
        assert_eq!(roster.required_session_role(), Some(SessionRole::Observer));
        let whoami = ClientRequest::Access(AccessRequest::GetCurrentParticipant);
        assert_eq!(whoami.required_capability(), None);
        assert_eq!(whoami.required_session_role(), None);
        assert_eq!(whoami.access_target(), None);

        let run_id = RunId::new();
        for (request, role) in [
            (
                ClientRequest::Run(RunRequest::GetAgentRun { run_id }),
                SessionRole::Observer,
            ),
            (
                ClientRequest::Run(RunRequest::PauseAgentRun { run_id }),
                SessionRole::Participant,
            ),
            (
                ClientRequest::Run(RunRequest::InterruptAgentRun { run_id }),
                SessionRole::Participant,
            ),
        ] {
            assert_eq!(request.required_session_role(), Some(role));
            assert_eq!(request.access_target(), Some(AccessTarget::Run(run_id)));
        }
        let attach = ClientRequest::Repository(RepositoryRequest::DetachSessionRepository {
            session_id,
            repository_id: RepositoryId::new(),
        });
        assert_eq!(attach.required_session_role(), Some(SessionRole::Owner));

        let response = ServerResponse::Access(AccessResponse::SessionRoster {
            session_id,
            entries: vec![RosterEntry {
                participant: ParticipantIdentity {
                    id: participant_id,
                    display_name: "Sam".to_owned(),
                },
                role: SessionRole::Observer,
                presence: PresenceStatus::Away,
                connections: 1,
                last_active_at: None,
            }],
        });
        let encoded = serde_json::to_value(&response).unwrap();
        assert_eq!(encoded["data"]["data"]["entries"][0]["presence"], "away");
        assert_eq!(
            serde_json::from_value::<ServerResponse>(encoded).unwrap(),
            response
        );
    }

    #[test]
    fn github_sign_in_requests_need_no_session_and_round_trip() {
        let start = ClientRequest::Access(AccessRequest::StartGitHubSignIn);
        assert_eq!(start.required_capability(), Some(Capability::GitHubSignIn));
        assert_eq!(start.required_session_role(), None);
        assert_eq!(start.access_target(), None);

        let status = ClientRequest::Access(AccessRequest::GetGitHubSignInStatus {
            login_id: "login-1".to_owned(),
        });
        assert_eq!(status.required_capability(), None);
        assert_eq!(status.required_session_role(), None);
        assert_eq!(status.access_target(), None);

        let sign_out = ClientRequest::Access(AccessRequest::SignOutGitHub);
        assert_eq!(sign_out.required_capability(), None);
        assert_eq!(sign_out.required_session_role(), None);

        let participant = ParticipantIdentity {
            id: loom_core::github_participant_id(141030),
            display_name: "bjornharrtell".to_owned(),
        };
        let responses = [
            ServerResponse::Access(AccessResponse::GitHubSignInStarted {
                login_id: "login-1".to_owned(),
                user_code: "ABCD-1234".to_owned(),
                verification_uri: "https://github.com/login/device".to_owned(),
                expires_in: 900,
                interval: 5,
            }),
            ServerResponse::Access(AccessResponse::GitHubSignInStatus {
                status: GitHubSignInStatus::Pending,
                resume_token: None,
            }),
            ServerResponse::Access(AccessResponse::GitHubSignInStatus {
                status: GitHubSignInStatus::SignedIn {
                    participant: participant.clone(),
                },
                resume_token: Some("person-credential".to_owned()),
            }),
            ServerResponse::Access(AccessResponse::GitHubSignInStatus {
                status: GitHubSignInStatus::Failed {
                    message: "expired".to_owned(),
                },
                resume_token: None,
            }),
            ServerResponse::Access(AccessResponse::SignedOutGitHub),
        ];
        for request in [start, status, sign_out] {
            let encoded = serde_json::to_value(&request).unwrap();
            assert_eq!(
                serde_json::from_value::<ClientRequest>(encoded).unwrap(),
                request
            );
        }
        for response in responses {
            let encoded = serde_json::to_value(&response).unwrap();
            assert_eq!(
                serde_json::from_value::<ServerResponse>(encoded).unwrap(),
                response
            );
        }
        // The credential is optional: a response without it leaves the field
        // out of the JSON, and one that has no field at all, as an older
        // worker sends, decodes as no credential.
        let without_credential = ServerResponse::Access(AccessResponse::GitHubSignInStatus {
            status: GitHubSignInStatus::SignedIn { participant },
            resume_token: None,
        });
        let encoded = serde_json::to_value(&without_credential).unwrap();
        assert!(encoded["data"]["data"].get("resume_token").is_none());
        let ServerResponse::Access(AccessResponse::GitHubSignInStatus { resume_token, .. }) =
            serde_json::from_value(encoded).unwrap()
        else {
            panic!("a sign-in status decodes as a sign-in status");
        };
        assert_eq!(resume_token, None);
        assert_eq!(
            serde_json::to_value(ClientRequest::Access(AccessRequest::SignOutGitHub)).unwrap(),
            serde_json::json!({"type": "access", "data": {"type": "sign_out_git_hub"}})
        );
    }

    #[test]
    fn collaboration_requests_need_participants_and_round_trip() {
        let session_id = AgentSessionId::new();
        let run_id = RunId::new();
        let expected = RunRevision {
            attempt_id: loom_core::RunAttemptId::new(),
            control_revision: 3,
            action_sequence: 1,
        };
        let peer = ClientRequest::Collaboration(CollaborationRequest::SendPeerMessage {
            session_id,
            text: "lunch?".to_owned(),
        });
        assert_eq!(
            peer.required_capability(),
            Some(Capability::SendPeerMessage)
        );
        assert_eq!(peer.required_session_role(), Some(SessionRole::Participant));
        assert_eq!(
            peer.access_target(),
            Some(AccessTarget::Session(session_id))
        );
        assert!(peer.is_retryable_mutation());
        for (request, capability) in [
            (
                ClientRequest::Collaboration(CollaborationRequest::DirectAgent {
                    run_id,
                    expected,
                    text: "go".to_owned(),
                }),
                Capability::ControlAgentRun,
            ),
            (
                ClientRequest::Collaboration(CollaborationRequest::AnswerAgentQuestion {
                    run_id,
                    expected,
                    text: "yes".to_owned(),
                }),
                Capability::ControlAgentRun,
            ),
            (
                ClientRequest::Collaboration(CollaborationRequest::ControlRun {
                    run_id,
                    expected,
                    action: RunControlAction::Pause,
                }),
                Capability::PauseAgentRun,
            ),
            (
                ClientRequest::Collaboration(CollaborationRequest::ControlRun {
                    run_id,
                    expected,
                    action: RunControlAction::Approve {
                        tool_call_id: ToolCallId::new(),
                    },
                }),
                Capability::ApproveAgentAction,
            ),
        ] {
            assert_eq!(request.required_capability(), Some(capability));
            assert_eq!(
                request.required_session_role(),
                Some(SessionRole::Participant)
            );
            assert_eq!(request.access_target(), Some(AccessTarget::Run(run_id)));
            let encoded = serde_json::to_value(&request).unwrap();
            assert_eq!(encoded["type"], "collaboration");
            assert_eq!(
                serde_json::from_value::<ClientRequest>(encoded).unwrap(),
                request
            );
        }
        let revision =
            ClientRequest::Collaboration(CollaborationRequest::GetRunRevision { run_id });
        assert_eq!(
            revision.required_session_role(),
            Some(SessionRole::Observer)
        );
        assert!(!revision.is_retryable_mutation());
    }

    #[test]
    fn project_snapshot_request_uses_project_capability_and_round_trips() {
        assert_eq!(CURRENT_PROTOCOL_VERSION, ProtocolVersion::new(11, 16));
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
            parent_project_id: None,
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
    fn create_sub_project_requires_session_creation_and_the_parent_owner_role() {
        let parent_session_id = AgentSessionId::new();
        let request = ClientRequest::Project(ProjectRequest::CreateSubProject {
            parent_session_id,
            name: "Branch out".into(),
        });
        // A sub project is a session of its own, so creating one draws on the
        // capability that creates sessions.
        assert_eq!(
            request.required_capability(),
            Some(Capability::CreateAgentSession)
        );
        // It changes the project it branches out of, so it needs the role that
        // may change that project.
        assert_eq!(request.required_session_role(), Some(SessionRole::Owner));
        assert_eq!(
            request.access_target(),
            Some(AccessTarget::Session(parent_session_id))
        );
        assert!(request.is_retryable_mutation());
        assert_eq!(
            decode_request(&encode_request(&RequestEnvelope::new(request.clone())).unwrap())
                .unwrap()
                .request,
            request
        );

        let sub_project_id = ProjectId::new();
        let response = ServerResponse::Project(ProjectResponse::SubProjectCreated {
            project_id: sub_project_id,
            session: Box::new(loom_core::AgentSessionSnapshot {
                id: AgentSessionId::from_uuid(*sub_project_id.as_uuid()),
                workspace_id: loom_core::WorkspaceId::new(),
                name: "Branch out".into(),
                goal: None,
                state: loom_core::AgentSessionState::Idle,
                created_at: loom_core::Timestamp::from_unix_millis(1),
                updated_at: loom_core::Timestamp::from_unix_millis(1),
                title_source: loom_core::SessionTitleSource::Auto,
            }),
        });
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

        // Abandoning a finished child is the same control request and shares its
        // capability and retryable-mutation classification.
        let abandon_request = ClientRequest::Project(ProjectRequest::ControlProjectChild {
            project_id,
            manager_session_id: parent,
            task_id: loom_core::TaskId::new(),
            action: ProjectChildControlAction::Abandon,
        });
        assert_eq!(
            abandon_request.required_capability(),
            Some(Capability::ControlProjectChild)
        );
        assert!(abandon_request.is_retryable_mutation());
        assert_eq!(
            decode_request(
                &encode_request(&RequestEnvelope::new(abandon_request.clone())).unwrap()
            )
            .unwrap()
            .request,
            abandon_request
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
                activity_detail: ActivityDetail::Full,
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
                next_after_sequence: None,
            }),
        );
        let mut encoded = serde_json::to_value(&response).unwrap();
        encoded["result"]["Ok"]["data"]
            .as_object_mut()
            .unwrap()
            .remove("stream_epoch");
        // A peer that predates the bounded batch omits the continuation cursor,
        // which has to read as an unbounded batch rather than as a failure.
        encoded["result"]["Ok"]["data"]
            .as_object_mut()
            .unwrap()
            .remove("next_after_sequence");
        let decoded: ResponseEnvelope = serde_json::from_value(encoded).unwrap();
        assert_eq!(
            decoded.result.unwrap(),
            ServerResponse::Events(EventsResponse::SessionEvents {
                events: Vec::new(),
                stream_epoch: None,
                next_after_sequence: None,
            })
        );
    }

    #[test]
    fn activity_detail_defaults_to_full_and_round_trips_every_level() {
        let session_id = AgentSessionId::new();
        let event_stream = |activity_detail| {
            ClientRequest::Events(EventsRequest::GetSessionEvents {
                activity_detail,
                session_id: Some(session_id),
                workspace_id: None,
                after_sequence: None,
                stream_epoch: None,
            })
        };

        // A request from a client that predates the field reads as `full`, so
        // the backend answers it exactly as it did before.
        let request = event_stream(ActivityDetail::Full);
        let mut encoded = serde_json::to_value(RequestEnvelope::new(request.clone())).unwrap();
        // The request is a tagged enum nested in the envelope, so its payload
        // sits two `data` levels down.
        encoded["request"]["data"]["data"]
            .as_object_mut()
            .unwrap()
            .remove("activity_detail");
        let decoded = decode_request(encoded.to_string().as_bytes()).unwrap();
        assert_eq!(decoded.request, request);

        // Every level round-trips, and encodes as the name a client sends.
        for (detail, name) in [
            (ActivityDetail::Replies, "replies"),
            (ActivityDetail::Activity, "activity"),
            (ActivityDetail::Full, "full"),
        ] {
            let request = event_stream(detail);
            let encoded = encode_request(&RequestEnvelope::new(request.clone())).unwrap();
            let json: serde_json::Value = serde_json::from_slice(&encoded).unwrap();
            assert_eq!(json["request"]["data"]["data"]["activity_detail"], name);
            assert_eq!(decode_request(&encoded).unwrap().request, request);
        }
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
    fn setting_a_session_title_source_round_trips_with_its_capability_and_role() {
        let session_id = AgentSessionId::new();
        let request = ClientRequest::Session(SessionRequest::SetAgentSessionTitleSource {
            session_id,
            source: loom_core::SessionTitleSource::Manual,
        });
        assert_eq!(
            request.required_capability(),
            Some(Capability::ControlAgentSession)
        );
        assert_eq!(
            request.required_session_role(),
            Some(loom_core::SessionRole::Owner)
        );
        assert_eq!(
            request.access_target(),
            Some(AccessTarget::Session(session_id))
        );
        assert!(request.is_retryable_mutation());
        assert_eq!(
            decode_request(&encode_request(&RequestEnvelope::new(request.clone())).unwrap())
                .unwrap()
                .request,
            request
        );
    }

    #[test]
    fn secret_bearing_provider_requests_are_not_retryable_mutations() {
        // Requests that carry a raw credential must not enter the durable
        // idempotency journal, which persists the full request payload.
        let configure_copilot = ClientRequest::Provider(ProviderRequest::ConfigureGitHubCopilot {
            access_token: "gho_copilot_secret".to_owned(),
        });
        assert!(!configure_copilot.is_retryable_mutation());

        let configure_repository =
            ClientRequest::Provider(ProviderRequest::ConfigureGitHubRepository {
                access_token: "gho_repository_secret".to_owned(),
            });
        assert!(!configure_repository.is_retryable_mutation());

        let configure_api_key = ClientRequest::Provider(ProviderRequest::ConfigureApiKeyProvider {
            provider_id: ProviderId::new("openai"),
            api_key: "sk-secret".to_owned(),
        });
        assert!(!configure_api_key.is_retryable_mutation());

        // A non-secret toggle may remain a retryable mutation.
        let write_access =
            ClientRequest::Provider(ProviderRequest::ConfigureGitHubWriteAccess { enabled: true });
        assert!(write_access.is_retryable_mutation());
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

        // The incremental feed carries the same additive continuation cursor as
        // a session batch, so a peer that predates it omits the field and has to
        // read it as an unbounded page rather than as a failure.
        let page = ServerResponse::Events(EventsResponse::WorkspaceEvents {
            workspace_id,
            events: vec![WorkspaceFeedEvent::Workspace(WorkspaceEventEnvelope {
                protocol_version: CURRENT_PROTOCOL_VERSION,
                sequence: EventSequence::new(5),
                workspace_id,
                event: WorkspaceEvent::Renamed {
                    name: "new name".to_owned(),
                },
            })],
            stream_epoch: Some("epoch".to_owned()),
            next_after_sequence: Some(EventSequence::new(5)),
        });
        let encoded = serde_json::to_vec(&page).unwrap();
        assert_eq!(
            serde_json::from_slice::<ServerResponse>(&encoded).unwrap(),
            page
        );
        let mut without_cursor = serde_json::to_value(&page).unwrap();
        // Two tagged layers: the `ServerResponse` and the `EventsResponse`
        // inside it, so the feed fields sit at the second `data`.
        let data = without_cursor["data"]["data"].as_object_mut().unwrap();
        data.remove("stream_epoch");
        data.remove("next_after_sequence");
        let decoded: ServerResponse = serde_json::from_value(without_cursor).unwrap();
        assert_eq!(
            decoded,
            ServerResponse::Events(EventsResponse::WorkspaceEvents {
                workspace_id,
                events: vec![WorkspaceFeedEvent::Workspace(WorkspaceEventEnvelope {
                    protocol_version: CURRENT_PROTOCOL_VERSION,
                    sequence: EventSequence::new(5),
                    workspace_id,
                    event: WorkspaceEvent::Renamed {
                        name: "new name".to_owned(),
                    },
                })],
                stream_epoch: None,
                next_after_sequence: None,
            })
        );
    }

    #[test]
    fn transcript_page_and_range_frames_round_trip_with_their_capability() {
        let session_id = AgentSessionId::new();
        let metadata_request =
            ClientRequest::Session(SessionRequest::GetAgentSessionSnapshotMetadata {
                activity_detail: ActivityDetail::Full,
                session_id,
            });
        assert_eq!(
            metadata_request.required_capability(),
            Some(Capability::ReadAgentSession)
        );
        let encoded = encode_request(&RequestEnvelope::new(metadata_request)).unwrap();
        assert_eq!(
            decode_request(&encoded).unwrap().request,
            ClientRequest::Session(SessionRequest::GetAgentSessionSnapshotMetadata {
                activity_detail: ActivityDetail::Full,
                session_id
            })
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
            activity_detail: ActivityDetail::Full,
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
                activity_detail: ActivityDetail::Full,
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
                    reasoning_content: Some("checked".to_owned()),
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
