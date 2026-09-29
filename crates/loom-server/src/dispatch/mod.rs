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
            ClientRequest::Negotiate { .. } | ClientRequest::DiscoverCapabilities => {
                unreachable!("capability requests are handled before dispatch")
            }
            ClientRequest::GetWorkerNodeStatus => self.control_dispatch(request, request_id),
            ClientRequest::CreateWorkspace { .. } => self.workspace_dispatch(request, request_id),
            ClientRequest::RegisterWorkspace { .. } => self.workspace_dispatch(request, request_id),
            ClientRequest::ListWorkspaces => self.workspace_dispatch(request, request_id),
            ClientRequest::RenameWorkspace { .. } => self.workspace_dispatch(request, request_id),
            ClientRequest::ListWorkspaceSessions { .. } => {
                self.workspace_dispatch(request, request_id)
            }
            ClientRequest::CreateAgentSessionInWorkspace { .. } => {
                self.workspace_dispatch(request, request_id)
            }
            ClientRequest::GetWorkspaceConfigForWorkspace { .. } => {
                self.workspace_dispatch(request, request_id)
            }
            ClientRequest::SetWorkspaceConfigForWorkspace { .. } => {
                self.workspace_dispatch(request, request_id)
            }
            ClientRequest::AttachSessionRepository { .. } => {
                self.repository_dispatch(request, request_id)
            }
            ClientRequest::ListSessionRepositories { .. } => {
                self.repository_dispatch(request, request_id)
            }
            ClientRequest::DetachSessionRepository { .. } => {
                self.repository_dispatch(request, request_id)
            }
            ClientRequest::ImportSessionDirectory { .. } => {
                self.filesystem_dispatch(request, request_id)
            }
            ClientRequest::AttachSessionDirectory { .. } => {
                self.filesystem_dispatch(request, request_id)
            }
            ClientRequest::ListSessionDirectories { .. } => {
                self.filesystem_dispatch(request, request_id)
            }
            ClientRequest::DetachSessionDirectory { .. } => {
                self.filesystem_dispatch(request, request_id)
            }
            ClientRequest::ListGitHubRepositories => self.repository_dispatch(request, request_id),
            ClientRequest::StartSessionAgentRun { .. } => {
                self.run_control_dispatch(request, request_id)
            }
            ClientRequest::StartSessionAgentRunWithOptions { .. } => {
                self.run_control_dispatch(request, request_id)
            }
            ClientRequest::GetSessionFilesystemSnapshot { .. } => {
                self.filesystem_dispatch(request, request_id)
            }
            ClientRequest::GetSessionFilesystemChanges { .. } => {
                self.filesystem_dispatch(request, request_id)
            }
            ClientRequest::ReadSessionFile { .. } => self.filesystem_dispatch(request, request_id),
            ClientRequest::ApplySessionFilesystemEdit { .. } => {
                self.filesystem_dispatch(request, request_id)
            }
            ClientRequest::TakeSessionFilesystemControl { .. } => {
                self.filesystem_dispatch(request, request_id)
            }
            ClientRequest::CreateSessionCheckpoint { .. } => {
                self.filesystem_dispatch(request, request_id)
            }
            ClientRequest::RevertSessionCheckpoint { .. } => {
                self.filesystem_dispatch(request, request_id)
            }
            ClientRequest::UndoSessionEdit { .. } => self.filesystem_dispatch(request, request_id),
            ClientRequest::GetSessionContextFiles { .. } => {
                self.filesystem_dispatch(request, request_id)
            }
            ClientRequest::GetSessionVcsStatus { .. } => {
                self.repository_dispatch(request, request_id)
            }
            ClientRequest::GetSessionVcsDiff { .. } => {
                self.repository_dispatch(request, request_id)
            }
            ClientRequest::GetSessionVcsBranches { .. } => {
                self.repository_dispatch(request, request_id)
            }
            ClientRequest::GetSessionVcsConflicts { .. } => {
                self.repository_dispatch(request, request_id)
            }
            ClientRequest::OpenSessionTerminal { .. } => {
                self.terminal_dispatch(request, request_id)
            }
            ClientRequest::WriteSessionTerminalInput { .. } => {
                self.terminal_dispatch(request, request_id)
            }
            ClientRequest::ResizeSessionTerminal { .. } => {
                self.terminal_dispatch(request, request_id)
            }
            ClientRequest::GetSessionTerminalEvents { .. } => {
                self.terminal_dispatch(request, request_id)
            }
            ClientRequest::CancelSessionTerminal { .. } => {
                self.terminal_dispatch(request, request_id)
            }
            ClientRequest::StartSessionTask { .. } => self.task_dispatch(request, request_id),
            ClientRequest::ListSessionTasks { .. } => self.task_dispatch(request, request_id),
            ClientRequest::GetSessionTask { .. } => self.task_dispatch(request, request_id),
            ClientRequest::GetSessionTaskEvents { .. } => self.task_dispatch(request, request_id),
            ClientRequest::CancelSessionTask { .. } => self.task_dispatch(request, request_id),
            ClientRequest::GetSessionTaskEvidence { .. } => self.task_dispatch(request, request_id),
            ClientRequest::SetSessionApprovalPolicy { .. } => {
                self.session_dispatch(request, request_id)
            }
            ClientRequest::GetAgentSession { .. } => self.session_dispatch(request, request_id),
            ClientRequest::GetAgentSessionSnapshot { .. } => {
                self.session_dispatch(request, request_id)
            }
            ClientRequest::GetAgentSessionSnapshotMetadata { .. } => {
                self.session_dispatch(request, request_id)
            }
            ClientRequest::GetAgentSessionInitialState { .. } => {
                self.session_dispatch(request, request_id)
            }
            ClientRequest::GetProjectSnapshot { .. } => self.project_dispatch(request, request_id),
            ClientRequest::GetProjectSnapshotForSession { .. } => {
                self.project_dispatch(request, request_id)
            }
            ClientRequest::SendProjectAgentMessage { .. } => {
                self.project_dispatch(request, request_id)
            }
            ClientRequest::ListProjectAgentMessages { .. } => {
                self.project_dispatch(request, request_id)
            }
            ClientRequest::ControlProjectChild { .. } => self.project_dispatch(request, request_id),
            ClientRequest::GetProjectChildReview { .. } => {
                self.project_dispatch(request, request_id)
            }
            ClientRequest::IntegrateProjectChild { .. } => {
                self.project_dispatch(request, request_id)
            }
            ClientRequest::CleanupProjectChildWorktree { .. } => {
                self.project_dispatch(request, request_id)
            }
            ClientRequest::RenameAgentSession { .. } => self.session_dispatch(request, request_id),
            ClientRequest::ArchiveAgentSession { .. } => self.session_dispatch(request, request_id),
            ClientRequest::GetSessionEvents { .. } => {
                self.session_events_dispatch(request, request_id)
            }
            ClientRequest::GetRecentSessionEvents { .. } => {
                self.session_events_dispatch(request, request_id)
            }
            ClientRequest::GetAgentRun { .. } => self.run_query_dispatch(request, request_id),
            ClientRequest::GetAgentRunMessagePage { .. } => {
                self.run_query_dispatch(request, request_id)
            }
            ClientRequest::GetAgentRunTranscriptPage { .. } => {
                self.run_query_dispatch(request, request_id)
            }
            ClientRequest::GetAgentRunMessageContentRange { .. } => {
                self.run_query_dispatch(request, request_id)
            }
            ClientRequest::GetAgentRunSnapshot { .. } => {
                self.run_query_dispatch(request, request_id)
            }
            ClientRequest::GetRunCheckpoint { .. } => self.run_query_dispatch(request, request_id),
            ClientRequest::ApproveAgentAction { .. } => {
                self.run_control_dispatch(request, request_id)
            }
            ClientRequest::RejectAgentAction { .. } => {
                self.run_control_dispatch(request, request_id)
            }
            ClientRequest::SendAgentMessage { .. } => {
                self.run_control_dispatch(request, request_id)
            }
            ClientRequest::InterruptAgentRun { .. } => {
                self.run_control_dispatch(request, request_id)
            }
            ClientRequest::RetryAgentStep { .. } => self.run_control_dispatch(request, request_id),
            ClientRequest::PauseAgentRun { .. } => self.run_control_dispatch(request, request_id),
            ClientRequest::ResumeAgentRun { .. } => self.run_control_dispatch(request, request_id),
            ClientRequest::RetryAgentFromCheckpoint { .. } => {
                self.run_control_dispatch(request, request_id)
            }
            ClientRequest::ForkAgentSession { .. } => self.session_dispatch(request, request_id),
            ClientRequest::ListModels => self.provider_dispatch(request, request_id),
            ClientRequest::ListProviders => self.provider_dispatch(request, request_id),
            ClientRequest::ConfigureGitHubCopilot { .. } => {
                self.provider_dispatch(request, request_id)
            }
            ClientRequest::ConfigureApiKeyProvider { .. } => {
                self.provider_dispatch(request, request_id)
            }
            ClientRequest::StartGitHubCopilotLogin => self.provider_dispatch(request, request_id),
            ClientRequest::GetGitHubCopilotLoginStatus { .. } => {
                self.provider_dispatch(request, request_id)
            }
            ClientRequest::DiscoverProviderModels { .. } => {
                self.provider_dispatch(request, request_id)
            }
            ClientRequest::GetProviderHealth { .. } => self.provider_dispatch(request, request_id),
            ClientRequest::GetRunUsage { .. } => self.usage_dispatch(request, request_id),
            ClientRequest::GetSessionUsage { .. } => self.usage_dispatch(request, request_id),
            ClientRequest::InspectAgentContext { .. } => {
                self.run_query_dispatch(request, request_id)
            }
            ClientRequest::AttachRunEvidence { .. } => {
                self.run_control_dispatch(request, request_id)
            }
        }
    }
}
