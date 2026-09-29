use super::*;

impl InProcessConnection {
    pub(super) fn project_dispatch(
        &self,
        request: ClientRequest,
        request_id: RequestId,
    ) -> Result<ServerResponse> {
        match request {
            ClientRequest::Project(ProjectRequest::GetProjectSnapshot { project_id }) => {
                Ok(ServerResponse::Project(ProjectResponse::ProjectSnapshot(
                    self.load_project_snapshot(project_id)?,
                )))
            }
            ClientRequest::Project(ProjectRequest::GetProjectSnapshotForSession { session_id }) => {
                Ok(ServerResponse::Project(ProjectResponse::ProjectSnapshot(
                    self.load_project_snapshot_for_session(session_id)?,
                )))
            }
            ClientRequest::Project(ProjectRequest::SendProjectAgentMessage { message }) => {
                self.send_project_agent_message(request_id, message)
            }
            ClientRequest::Project(ProjectRequest::ListProjectAgentMessages {
                project_id,
                session_id,
                after_project_sequence,
                limit,
            }) => self.list_project_agent_messages(
                project_id,
                session_id,
                after_project_sequence.unwrap_or(0),
                limit,
            ),
            ClientRequest::Project(ProjectRequest::ControlProjectChild {
                project_id,
                manager_session_id,
                task_id,
                action,
            }) => {
                let (task, run) =
                    self.control_project_child(manager_session_id, project_id, task_id, action)?;
                Ok(ServerResponse::Project(
                    ProjectResponse::ProjectChildControlled { task, run },
                ))
            }
            ClientRequest::Project(ProjectRequest::GetProjectChildReview {
                project_id,
                manager_session_id,
                task_id,
            }) => self.get_project_child_review(project_id, manager_session_id, task_id),
            ClientRequest::Project(ProjectRequest::IntegrateProjectChild {
                project_id,
                manager_session_id,
                task_id,
                expected_parent_revision,
            }) => self.integrate_project_child(
                request_id,
                project_id,
                manager_session_id,
                task_id,
                expected_parent_revision,
            ),
            ClientRequest::Project(ProjectRequest::CleanupProjectChildWorktree {
                project_id,
                manager_session_id,
                task_id,
                disposition,
            }) => self.cleanup_project_child_worktree(
                project_id,
                manager_session_id,
                task_id,
                disposition,
            ),
            _ => unreachable!("request was routed to the wrong dispatch domain"),
        }
    }
}
