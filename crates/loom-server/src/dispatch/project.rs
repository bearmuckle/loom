use super::*;

impl InProcessConnection {
    pub(super) fn project_dispatch(
        &self,
        request: ClientRequest,
        request_id: RequestId,
    ) -> Result<ServerResponse> {
        match request {
            ClientRequest::GetProjectSnapshot { project_id } => Ok(
                ServerResponse::ProjectSnapshot(self.load_project_snapshot(project_id)?),
            ),
            ClientRequest::GetProjectSnapshotForSession { session_id } => {
                Ok(ServerResponse::ProjectSnapshot(
                    self.load_project_snapshot_for_session(session_id)?,
                ))
            }
            ClientRequest::SendProjectAgentMessage { message } => {
                self.send_project_agent_message(request_id, message)
            }
            ClientRequest::ListProjectAgentMessages {
                project_id,
                session_id,
                after_project_sequence,
                limit,
            } => self.list_project_agent_messages(
                project_id,
                session_id,
                after_project_sequence.unwrap_or(0),
                limit,
            ),
            ClientRequest::ControlProjectChild {
                project_id,
                manager_session_id,
                task_id,
                action,
            } => {
                let (task, run) =
                    self.control_project_child(manager_session_id, project_id, task_id, action)?;
                Ok(ServerResponse::ProjectChildControlled { task, run })
            }
            ClientRequest::GetProjectChildReview {
                project_id,
                manager_session_id,
                task_id,
            } => self.get_project_child_review(project_id, manager_session_id, task_id),
            ClientRequest::IntegrateProjectChild {
                project_id,
                manager_session_id,
                task_id,
                expected_parent_revision,
            } => self.integrate_project_child(
                request_id,
                project_id,
                manager_session_id,
                task_id,
                expected_parent_revision,
            ),
            ClientRequest::CleanupProjectChildWorktree {
                project_id,
                manager_session_id,
                task_id,
                disposition,
            } => self.cleanup_project_child_worktree(
                project_id,
                manager_session_id,
                task_id,
                disposition,
            ),
            _ => unreachable!("request was routed to the wrong dispatch domain"),
        }
    }
}
