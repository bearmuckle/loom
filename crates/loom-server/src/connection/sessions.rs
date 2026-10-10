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
        if session.state == AgentSessionState::Archived {
            log::warn!("[loom-server] refusing to archive session {session_id}: already archived");
            return Err(LoomError::invalid_state(
                "agent session is already archived",
            ));
        }
        if self.backend.persistence.is_none() {
            // Without persistence there is no project snapshot, so a project
            // root is archived as a plain session and its children are not
            // cascaded.
            log::debug!(
                "[loom-server] archiving session {session_id} without persistence; project cascade is unavailable"
            );
        }
        let project = self
            .backend
            .persistence
            .as_ref()
            .map(|persistence| persistence.load_project_snapshot_for_session(session_id))
            .transpose()?
            .flatten()
            .filter(|project| project.root_session_id == session_id);
        // Resolve the descendant sessions and validate every one of them with
        // its live state before mutating any, so a rejected child cannot leave
        // a partially archived project tree behind.
        let descendants = if let Some(project) = &project {
            let unfinished_tasks = project
                .tasks
                .iter()
                .filter(|task| {
                    !matches!(
                        task.status,
                        loom_core::DelegatedTaskStatus::Completed
                            | loom_core::DelegatedTaskStatus::Failed
                            | loom_core::DelegatedTaskStatus::Cancelled
                    )
                })
                .map(|task| format!("{} ({:?})", task.child_name, task.status))
                .collect::<Vec<_>>();
            let mut descendants = Vec::new();
            let mut active_children = Vec::new();
            for agent in &project.agents {
                if agent.session_id == session_id {
                    continue;
                }
                let state = self.backend.sessions()?.get(agent.session_id)?.state;
                if state == AgentSessionState::Archived {
                    continue;
                }
                if matches!(
                    state,
                    AgentSessionState::Queued
                        | AgentSessionState::Planning
                        | AgentSessionState::AwaitingApproval
                        | AgentSessionState::Paused
                        | AgentSessionState::Executing
                        | AgentSessionState::Evaluating
                        | AgentSessionState::NeedsInput
                ) {
                    active_children.push(format!("{} ({state:?})", agent.session_id));
                } else {
                    descendants.push((agent.session_id, agent.depth));
                }
            }
            if !unfinished_tasks.is_empty() || !active_children.is_empty() {
                log::warn!(
                    "[loom-server] refusing to archive project root {session_id}: unfinished child tasks {unfinished_tasks:?}, active child sessions {active_children:?}"
                );
                return Err(LoomError::new(
                    ErrorCode::InvalidState,
                    "finish or cancel every child task before archiving this project",
                    false,
                ));
            }
            descendants.sort_by_key(|(_, depth)| std::cmp::Reverse(*depth));
            log::info!(
                "[loom-server] archiving project root {session_id} with {} descendant session(s)",
                descendants.len()
            );
            descendants
        } else {
            log::info!("[loom-server] archiving session {session_id}");
            Vec::new()
        };
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
            let registered_run = self
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
                .map(|(run_id, _)| run_id);
            // A run deferred by restore has no in-memory handle. Resolve the
            // persisted latest run so it can still be stopped.
            let latest_run = self
                .backend
                .persistence
                .as_ref()
                .map(|persistence| persistence.load_latest_run_summary_for_session(session_id))
                .transpose()?
                .flatten();
            let run_id = registered_run.or_else(|| {
                latest_run
                    .as_ref()
                    .filter(|summary| {
                        !matches!(
                            summary.snapshot.state,
                            AgentRunState::Completed
                                | AgentRunState::Failed
                                | AgentRunState::Cancelled
                        )
                    })
                    .map(|summary| summary.snapshot.id)
            });
            match run_id {
                Some(run_id) => {
                    log::info!(
                        "[loom-server] stopping run {run_id} before archiving session {session_id}"
                    );
                    // Restore a deferred run on demand so it can be interrupted.
                    if let Err(error) = self.run_handle(run_id) {
                        log::warn!(
                            "[loom-server] failed to restore run {run_id} before archiving session {session_id}: {}",
                            error.message
                        );
                        return Err(error);
                    }
                    if let Err(error) = self.stop_run(run_id, RunStop::Interrupt) {
                        log::warn!(
                            "[loom-server] failed to stop run {run_id} before archiving session {session_id}: {}",
                            error.message
                        );
                        return Err(error);
                    }
                }
                None => {
                    // No non-terminal run exists; the persisted session state is
                    // stale (for example left active by an earlier crash).
                    // Reconcile it to the latest run's terminal state, or Idle,
                    // so archiving can proceed.
                    let stopped_state = latest_run
                        .as_ref()
                        .map(|summary| session_state_for_run_state(summary.snapshot.state))
                        .unwrap_or(AgentSessionState::Idle);
                    log::warn!(
                        "[loom-server] session {session_id} was {:?} with no active run; resetting to {stopped_state:?} before archiving",
                        session.state
                    );
                    let (_, record) = self
                        .backend
                        .sessions()?
                        .transition(session_id, stopped_state)?;
                    self.backend.journal()?.append_session(record);
                }
            }
        }

        for (child_id, depth) in descendants {
            log::info!(
                "[loom-server] archiving descendant session {child_id} (depth {depth}) of project root {session_id}"
            );
            let (_, record) = self
                .backend
                .sessions()?
                .archive(child_id)
                .inspect_err(|error| {
                    log::warn!(
                        "[loom-server] failed to archive descendant session {child_id} of project root {session_id}: {}",
                        error.message
                    );
                })?;
            self.backend.journal()?.append_session(record);
        }

        let (snapshot, record) =
            self.backend
                .sessions()?
                .archive(session_id)
                .inspect_err(|error| {
                    log::warn!(
                        "[loom-server] failed to archive session {session_id}: {}",
                        error.message
                    );
                })?;
        self.backend.journal()?.append_session(record);
        log::info!("[loom-server] archived session {session_id}");
        Ok(ServerResponse::Session(
            SessionResponse::AgentSessionArchived(snapshot),
        ))
    }
}

impl InProcessConnection {
    /// Permanently removes archived agent sessions and their stored history.
    ///
    /// The backend owns the deletion, because the retention sweep performs the
    /// same work without a connection; this wrapper only forwards the request.
    pub(crate) fn delete_archived_session(
        &self,
        session_id: AgentSessionId,
        force: bool,
    ) -> Result<ServerResponse> {
        self.backend.delete_archived_session(session_id, force)
    }

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
