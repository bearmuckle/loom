use super::*;

impl InProcessConnection {
    pub(crate) fn session_initial_state(
        &self,
        session_id: AgentSessionId,
    ) -> Result<AgentSessionInitialState> {
        for _ in 0..4 {
            let before = self.latest_session_event_sequence(session_id)?;
            let mut projection = self.session_snapshot_projection(session_id, false)?;
            let after = self.latest_session_event_sequence(session_id)?;
            if before == after {
                projection.latest_sequence = after;
                return Ok(AgentSessionInitialState {
                    projection,
                    cursor: after,
                });
            }
        }
        Err(LoomError::new(
            ErrorCode::Conflict,
            "session changed while reading initial state; retry",
            true,
        ))
    }

    pub(crate) fn policy(&self, session_id: AgentSessionId) -> Result<ApprovalPolicy> {
        if let Some(policy) = self.backend.session_policies()?.get(&session_id).cloned() {
            return Ok(policy);
        }
        Ok(ApprovalPolicy::auto_approve())
    }

    pub(crate) fn auto_approve_actions(&self, session_id: AgentSessionId) -> Result<bool> {
        let settings = self.backend.auto_approve_actions()?;
        if let Some(auto_approve_actions) = settings.get(&session_id) {
            return Ok(*auto_approve_actions);
        }
        drop(settings);
        Ok(self.policy(session_id)? == ApprovalPolicy::auto_approve())
    }

    pub(crate) fn archive_session(&self, session_id: AgentSessionId) -> Result<ServerResponse> {
        let session = self.backend.sessions()?.get(session_id)?;
        let project = self
            .backend
            .persistence
            .as_ref()
            .map(|persistence| persistence.load_project_snapshot_for_session(session_id))
            .transpose()?
            .flatten()
            .filter(|project| project.root_session_id == session_id);
        if let Some(project) = &project {
            let unfinished_task = project.tasks.iter().any(|task| {
                !matches!(
                    task.status,
                    loom_core::DelegatedTaskStatus::Completed
                        | loom_core::DelegatedTaskStatus::Failed
                        | loom_core::DelegatedTaskStatus::Cancelled
                )
            });
            let unfinished_child = project.agents.iter().any(|agent| {
                agent.session_id != session_id
                    && matches!(
                        agent.state,
                        AgentSessionState::Queued
                            | AgentSessionState::Planning
                            | AgentSessionState::AwaitingApproval
                            | AgentSessionState::Paused
                            | AgentSessionState::Executing
                            | AgentSessionState::Evaluating
                            | AgentSessionState::NeedsInput
                    )
            });
            if unfinished_task || unfinished_child {
                return Err(LoomError::new(
                    ErrorCode::InvalidState,
                    "finish or cancel every child task before archiving this project",
                    false,
                ));
            }
        }
        if matches!(
            session.state,
            AgentSessionState::Queued
                | AgentSessionState::Planning
                | AgentSessionState::AwaitingApproval
                | AgentSessionState::Paused
                | AgentSessionState::Executing
                | AgentSessionState::Evaluating
                | AgentSessionState::NeedsInput
        ) {
            let run_id = self
                .backend
                .runs()?
                .iter()
                .filter(|(_, handle)| handle.session_id == session_id)
                .map(|(run_id, handle)| (*run_id, handle.snapshot()))
                .filter(|(_, snapshot)| {
                    !matches!(
                        snapshot.state,
                        AgentRunState::Completed | AgentRunState::Failed | AgentRunState::Cancelled
                    )
                })
                .max_by_key(|(_, snapshot)| snapshot.updated_at)
                .map(|(run_id, _)| run_id)
                .ok_or_else(|| {
                    LoomError::invalid_state(
                        "running agent sessions must be stopped before archiving",
                    )
                })?;
            self.stop_run(run_id, RunStop::Interrupt)?;
        }

        if let Some(project) = project {
            let mut descendants = project
                .agents
                .into_iter()
                .filter(|agent| {
                    agent.session_id != session_id && agent.state != AgentSessionState::Archived
                })
                .collect::<Vec<_>>();
            descendants.sort_by_key(|agent| std::cmp::Reverse(agent.depth));
            for agent in descendants {
                let (_, record) = self.backend.sessions()?.archive(agent.session_id)?;
                self.backend.journal()?.append_session(record);
            }
        }

        let (snapshot, record) = self.backend.sessions()?.archive(session_id)?;
        self.backend.journal()?.append_session(record);
        Ok(ServerResponse::AgentSessionArchived(snapshot))
    }
}
