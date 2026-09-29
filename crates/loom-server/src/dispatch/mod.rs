//! Per-domain request dispatch.
//!
//! The central [`InProcessConnection::dispatch_request`] router maps each
//! [`ClientRequest`] variant to the module that owns its domain logic. Each
//! domain module implements `<domain>_dispatch` over its slice of requests.

use super::*;

mod control;
mod filesystem;
mod project;
mod provider;
mod repository;
mod run_control;
mod run_query;
mod session;
mod session_events;
mod task;
mod terminal;
mod usage;
mod workspace;

impl InProcessConnection {
    pub(super) fn dispatch_request(
        &self,
        request: ClientRequest,
        request_id: RequestId,
    ) -> Result<ServerResponse> {
        match request {
            ClientRequest::Control(ControlRequest::Negotiate { .. })
            | ClientRequest::Control(ControlRequest::DiscoverCapabilities) => {
                unreachable!("capability requests are handled before dispatch")
            }
            ClientRequest::Control(ControlRequest::GetWorkerNodeStatus) => {
                self.control_dispatch(request, request_id)
            }
            ClientRequest::Workspace(WorkspaceRequest::CreateWorkspace { .. }) => {
                self.workspace_dispatch(request, request_id)
            }
            ClientRequest::Workspace(WorkspaceRequest::RegisterWorkspace { .. }) => {
                self.workspace_dispatch(request, request_id)
            }
            ClientRequest::Workspace(WorkspaceRequest::ListWorkspaces) => {
                self.workspace_dispatch(request, request_id)
            }
            ClientRequest::Workspace(WorkspaceRequest::RenameWorkspace { .. }) => {
                self.workspace_dispatch(request, request_id)
            }
            ClientRequest::Workspace(WorkspaceRequest::ListWorkspaceSessions { .. }) => {
                self.workspace_dispatch(request, request_id)
            }
            ClientRequest::Workspace(WorkspaceRequest::CreateAgentSessionInWorkspace {
                ..
            }) => self.workspace_dispatch(request, request_id),
            ClientRequest::Workspace(WorkspaceRequest::GetWorkspaceConfigForWorkspace {
                ..
            }) => self.workspace_dispatch(request, request_id),
            ClientRequest::Workspace(WorkspaceRequest::SetWorkspaceConfigForWorkspace {
                ..
            }) => self.workspace_dispatch(request, request_id),
            ClientRequest::Repository(RepositoryRequest::AttachSessionRepository { .. }) => {
                self.repository_dispatch(request, request_id)
            }
            ClientRequest::Repository(RepositoryRequest::ListSessionRepositories { .. }) => {
                self.repository_dispatch(request, request_id)
            }
            ClientRequest::Repository(RepositoryRequest::DetachSessionRepository { .. }) => {
                self.repository_dispatch(request, request_id)
            }
            ClientRequest::Filesystem(FilesystemRequest::ImportSessionDirectory { .. }) => {
                self.filesystem_dispatch(request, request_id)
            }
            ClientRequest::Filesystem(FilesystemRequest::AttachSessionDirectory { .. }) => {
                self.filesystem_dispatch(request, request_id)
            }
            ClientRequest::Filesystem(FilesystemRequest::ListSessionDirectories { .. }) => {
                self.filesystem_dispatch(request, request_id)
            }
            ClientRequest::Filesystem(FilesystemRequest::DetachSessionDirectory { .. }) => {
                self.filesystem_dispatch(request, request_id)
            }
            ClientRequest::Repository(RepositoryRequest::ListGitHubRepositories) => {
                self.repository_dispatch(request, request_id)
            }
            ClientRequest::Run(RunRequest::StartSessionAgentRun { .. }) => {
                self.run_control_dispatch(request, request_id)
            }
            ClientRequest::Run(RunRequest::StartSessionAgentRunWithOptions { .. }) => {
                self.run_control_dispatch(request, request_id)
            }
            ClientRequest::Filesystem(FilesystemRequest::GetSessionFilesystemSnapshot {
                ..
            }) => self.filesystem_dispatch(request, request_id),
            ClientRequest::Filesystem(FilesystemRequest::GetSessionFilesystemChanges {
                ..
            }) => self.filesystem_dispatch(request, request_id),
            ClientRequest::Filesystem(FilesystemRequest::ReadSessionFile { .. }) => {
                self.filesystem_dispatch(request, request_id)
            }
            ClientRequest::Filesystem(FilesystemRequest::ApplySessionFilesystemEdit { .. }) => {
                self.filesystem_dispatch(request, request_id)
            }
            ClientRequest::Filesystem(FilesystemRequest::TakeSessionFilesystemControl {
                ..
            }) => self.filesystem_dispatch(request, request_id),
            ClientRequest::Filesystem(FilesystemRequest::CreateSessionCheckpoint { .. }) => {
                self.filesystem_dispatch(request, request_id)
            }
            ClientRequest::Filesystem(FilesystemRequest::RevertSessionCheckpoint { .. }) => {
                self.filesystem_dispatch(request, request_id)
            }
            ClientRequest::Filesystem(FilesystemRequest::UndoSessionEdit { .. }) => {
                self.filesystem_dispatch(request, request_id)
            }
            ClientRequest::Filesystem(FilesystemRequest::GetSessionContextFiles { .. }) => {
                self.filesystem_dispatch(request, request_id)
            }
            ClientRequest::Repository(RepositoryRequest::GetSessionVcsStatus { .. }) => {
                self.repository_dispatch(request, request_id)
            }
            ClientRequest::Repository(RepositoryRequest::GetSessionVcsDiff { .. }) => {
                self.repository_dispatch(request, request_id)
            }
            ClientRequest::Repository(RepositoryRequest::GetSessionVcsBranches { .. }) => {
                self.repository_dispatch(request, request_id)
            }
            ClientRequest::Repository(RepositoryRequest::GetSessionVcsConflicts { .. }) => {
                self.repository_dispatch(request, request_id)
            }
            ClientRequest::Terminal(TerminalRequest::OpenSessionTerminal { .. }) => {
                self.terminal_dispatch(request, request_id)
            }
            ClientRequest::Terminal(TerminalRequest::WriteSessionTerminalInput { .. }) => {
                self.terminal_dispatch(request, request_id)
            }
            ClientRequest::Terminal(TerminalRequest::ResizeSessionTerminal { .. }) => {
                self.terminal_dispatch(request, request_id)
            }
            ClientRequest::Terminal(TerminalRequest::GetSessionTerminalEvents { .. }) => {
                self.terminal_dispatch(request, request_id)
            }
            ClientRequest::Terminal(TerminalRequest::CancelSessionTerminal { .. }) => {
                self.terminal_dispatch(request, request_id)
            }
            ClientRequest::Task(TaskRequest::StartSessionTask { .. }) => {
                self.task_dispatch(request, request_id)
            }
            ClientRequest::Task(TaskRequest::ListSessionTasks { .. }) => {
                self.task_dispatch(request, request_id)
            }
            ClientRequest::Task(TaskRequest::GetSessionTask { .. }) => {
                self.task_dispatch(request, request_id)
            }
            ClientRequest::Task(TaskRequest::GetSessionTaskEvents { .. }) => {
                self.task_dispatch(request, request_id)
            }
            ClientRequest::Task(TaskRequest::CancelSessionTask { .. }) => {
                self.task_dispatch(request, request_id)
            }
            ClientRequest::Task(TaskRequest::GetSessionTaskEvidence { .. }) => {
                self.task_dispatch(request, request_id)
            }
            ClientRequest::Session(SessionRequest::SetSessionApprovalPolicy { .. }) => {
                self.session_dispatch(request, request_id)
            }
            ClientRequest::Session(SessionRequest::GetAgentSession { .. }) => {
                self.session_dispatch(request, request_id)
            }
            ClientRequest::Session(SessionRequest::GetAgentSessionSnapshot { .. }) => {
                self.session_dispatch(request, request_id)
            }
            ClientRequest::Session(SessionRequest::GetAgentSessionSnapshotMetadata { .. }) => {
                self.session_dispatch(request, request_id)
            }
            ClientRequest::Session(SessionRequest::GetAgentSessionInitialState { .. }) => {
                self.session_dispatch(request, request_id)
            }
            ClientRequest::Project(ProjectRequest::GetProjectSnapshot { .. }) => {
                self.project_dispatch(request, request_id)
            }
            ClientRequest::Project(ProjectRequest::GetProjectSnapshotForSession { .. }) => {
                self.project_dispatch(request, request_id)
            }
            ClientRequest::Project(ProjectRequest::SendProjectAgentMessage { .. }) => {
                self.project_dispatch(request, request_id)
            }
            ClientRequest::Project(ProjectRequest::ListProjectAgentMessages { .. }) => {
                self.project_dispatch(request, request_id)
            }
            ClientRequest::Project(ProjectRequest::ControlProjectChild { .. }) => {
                self.project_dispatch(request, request_id)
            }
            ClientRequest::Project(ProjectRequest::GetProjectChildReview { .. }) => {
                self.project_dispatch(request, request_id)
            }
            ClientRequest::Project(ProjectRequest::IntegrateProjectChild { .. }) => {
                self.project_dispatch(request, request_id)
            }
            ClientRequest::Project(ProjectRequest::CleanupProjectChildWorktree { .. }) => {
                self.project_dispatch(request, request_id)
            }
            ClientRequest::Session(SessionRequest::RenameAgentSession { .. }) => {
                self.session_dispatch(request, request_id)
            }
            ClientRequest::Session(SessionRequest::ArchiveAgentSession { .. }) => {
                self.session_dispatch(request, request_id)
            }
            ClientRequest::Events(EventsRequest::GetSessionEvents { .. }) => {
                self.session_events_dispatch(request, request_id)
            }
            ClientRequest::Events(EventsRequest::GetRecentSessionEvents { .. }) => {
                self.session_events_dispatch(request, request_id)
            }
            ClientRequest::Run(RunRequest::GetAgentRun { .. }) => {
                self.run_query_dispatch(request, request_id)
            }
            ClientRequest::Run(RunRequest::GetAgentRunMessagePage { .. }) => {
                self.run_query_dispatch(request, request_id)
            }
            ClientRequest::Run(RunRequest::GetAgentRunTranscriptPage { .. }) => {
                self.run_query_dispatch(request, request_id)
            }
            ClientRequest::Run(RunRequest::GetAgentRunMessageContentRange { .. }) => {
                self.run_query_dispatch(request, request_id)
            }
            ClientRequest::Run(RunRequest::GetAgentRunSnapshot { .. }) => {
                self.run_query_dispatch(request, request_id)
            }
            ClientRequest::Run(RunRequest::GetRunCheckpoint { .. }) => {
                self.run_query_dispatch(request, request_id)
            }
            ClientRequest::Run(RunRequest::ApproveAgentAction { .. }) => {
                self.run_control_dispatch(request, request_id)
            }
            ClientRequest::Run(RunRequest::RejectAgentAction { .. }) => {
                self.run_control_dispatch(request, request_id)
            }
            ClientRequest::Run(RunRequest::SendAgentMessage { .. }) => {
                self.run_control_dispatch(request, request_id)
            }
            ClientRequest::Run(RunRequest::InterruptAgentRun { .. }) => {
                self.run_control_dispatch(request, request_id)
            }
            ClientRequest::Run(RunRequest::RetryAgentStep { .. }) => {
                self.run_control_dispatch(request, request_id)
            }
            ClientRequest::Run(RunRequest::PauseAgentRun { .. }) => {
                self.run_control_dispatch(request, request_id)
            }
            ClientRequest::Run(RunRequest::ResumeAgentRun { .. }) => {
                self.run_control_dispatch(request, request_id)
            }
            ClientRequest::Run(RunRequest::RetryAgentFromCheckpoint { .. }) => {
                self.run_control_dispatch(request, request_id)
            }
            ClientRequest::Session(SessionRequest::ForkAgentSession { .. }) => {
                self.session_dispatch(request, request_id)
            }
            ClientRequest::Provider(ProviderRequest::ListModels) => {
                self.provider_dispatch(request, request_id)
            }
            ClientRequest::Provider(ProviderRequest::ListProviders) => {
                self.provider_dispatch(request, request_id)
            }
            ClientRequest::Provider(ProviderRequest::ConfigureGitHubCopilot { .. }) => {
                self.provider_dispatch(request, request_id)
            }
            ClientRequest::Provider(ProviderRequest::ConfigureApiKeyProvider { .. }) => {
                self.provider_dispatch(request, request_id)
            }
            ClientRequest::Provider(ProviderRequest::StartGitHubCopilotLogin) => {
                self.provider_dispatch(request, request_id)
            }
            ClientRequest::Provider(ProviderRequest::GetGitHubCopilotLoginStatus { .. }) => {
                self.provider_dispatch(request, request_id)
            }
            ClientRequest::Provider(ProviderRequest::ConfigureGitHubWriteAccess { .. }) => {
                self.provider_dispatch(request, request_id)
            }
            ClientRequest::Provider(ProviderRequest::GetGitHubWriteAccess) => {
                self.provider_dispatch(request, request_id)
            }
            ClientRequest::Provider(ProviderRequest::DiscoverProviderModels { .. }) => {
                self.provider_dispatch(request, request_id)
            }
            ClientRequest::Provider(ProviderRequest::GetProviderHealth { .. }) => {
                self.provider_dispatch(request, request_id)
            }
            ClientRequest::Usage(UsageRequest::GetRunUsage { .. }) => {
                self.usage_dispatch(request, request_id)
            }
            ClientRequest::Usage(UsageRequest::GetSessionUsage { .. }) => {
                self.usage_dispatch(request, request_id)
            }
            ClientRequest::Context(ContextRequest::InspectAgentContext { .. }) => {
                self.run_query_dispatch(request, request_id)
            }
            ClientRequest::Run(RunRequest::AttachRunEvidence { .. }) => {
                self.run_control_dispatch(request, request_id)
            }
        }
    }
}
