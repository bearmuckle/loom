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
        Ok(ServerResponse::Session(
            SessionResponse::AgentSessionArchived(snapshot),
        ))
    }
}

impl InProcessConnection {
    pub(crate) fn session_snapshot_projection(
        &self,
        session_id: AgentSessionId,
        include_messages: bool,
    ) -> Result<AgentSessionSnapshotProjection> {
        let session = self.backend.sessions()?.get(session_id)?;
        let (loaded_ids, latest_loaded) = {
            let runs = self.backend.runs()?;
            let loaded_ids = runs.keys().copied().collect::<BTreeSet<_>>();
            let latest = runs
                .values()
                .filter(|handle| handle.session_id == session_id)
                .max_by_key(|handle| handle.snapshot().updated_at)
                .cloned();
            (loaded_ids, latest)
        };
        let persisted_projection = match &self.backend.persistence {
            Some(persistence) if !include_messages => {
                Some(persistence.load_session_projection_read(session_id)?)
            }
            _ => None,
        };
        let latest_persisted = if let Some(projection) = &persisted_projection {
            projection
                .latest_run
                .as_ref()
                .map(|summary| PersistedRunSummary {
                    snapshot: summary.snapshot.clone(),
                    usage: summary.usage.clone(),
                })
                .filter(|summary| !loaded_ids.contains(&summary.snapshot.id))
        } else {
            match &self.backend.persistence {
                Some(persistence) => persistence
                    .load_latest_run_summary_for_session(session_id)?
                    .map(|summary| PersistedRunSummary {
                        snapshot: summary.snapshot,
                        usage: summary.usage,
                    })
                    .filter(|summary| !loaded_ids.contains(&summary.snapshot.id)),
                None => None,
            }
        };
        let load_selected_persisted = |summary: &PersistedRunSummary| {
            if let Some(projection) = &persisted_projection {
                self.load_persisted_run_state_from_projection(summary, projection)
            } else {
                self.load_persisted_run_state(summary, include_messages)
            }
        };
        let active_run = match (latest_loaded, latest_persisted) {
            (Some(handle), Some(summary)) => {
                let projection = handle.snapshot_projection(include_messages);
                if projection.run.updated_at >= summary.snapshot.updated_at {
                    Some(projection)
                } else {
                    Some(run_snapshot_projection(&load_selected_persisted(&summary)?))
                }
            }
            (Some(handle), None) => Some(handle.snapshot_projection(include_messages)),
            (None, Some(summary)) => {
                Some(run_snapshot_projection(&load_selected_persisted(&summary)?))
            }
            (None, None) => None,
        };
        let latest_sequence = persisted_projection
            .as_ref()
            .and_then(|projection| projection.latest_sequence)
            .map(Ok)
            .unwrap_or_else(|| self.latest_session_event_sequence(session_id))?;
        let approval_policy = self.policy(session_id)?;
        let auto_approve_actions = self.auto_approve_actions(session_id)?;
        Ok(AgentSessionSnapshotProjection {
            session,
            active_run,
            latest_sequence,
            approval_policy,
            auto_approve_actions,
        })
    }
}
