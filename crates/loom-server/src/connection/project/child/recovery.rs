use super::*;

impl InProcessConnection {
    pub(crate) fn recover_pending_project_cancellation_cascades(&self) -> Result<()> {
        let Some(persistence) = self.backend.persistence.as_ref() else {
            return Ok(());
        };
        for cascade in persistence.list_pending_project_cancellation_cascades()? {
            self.apply_project_cancellation_cascade(&cascade)?;
        }
        Ok(())
    }

    /// Finishes a pending cancellation cascade for one project, if any.
    ///
    /// Startup recovery ([`Self::recover_pending_project_cancellation_cascades`])
    /// is the only other path, so a long-running backend that hit the pause
    /// would otherwise never clear the intent. Callers must not hold the project
    /// admission lock because [`Self::apply_project_cancellation_cascade`]
    /// acquires it.
    pub(crate) fn recover_project_cancellation_cascade(
        &self,
        project_id: ProjectId,
    ) -> Result<bool> {
        let Some(persistence) = self.backend.persistence.as_ref() else {
            return Ok(false);
        };
        let mut recovered = false;
        for cascade in persistence.list_pending_project_cancellation_cascades()? {
            if cascade.project_id == project_id {
                self.apply_project_cancellation_cascade(&cascade)?;
                recovered = true;
            }
        }
        Ok(recovered)
    }

    pub(crate) fn apply_project_cancellation_cascade(
        &self,
        cascade: &ProjectCancellationCascadeRecord,
    ) -> Result<(loom_core::DelegatedTaskRecord, Option<AgentRunSnapshot>)> {
        let persistence = self.backend.persistence.as_ref().ok_or_else(|| {
            LoomError::new(
                ErrorCode::UnsupportedCapability,
                "project child cancellation requires durable storage",
                false,
            )
        })?;
        let project_admission = self.backend.admissions.project(cascade.project_id)?;
        let _project_admission_guard = project_admission.lock().map_err(|_| {
            LoomError::new(
                ErrorCode::Internal,
                "project scheduling lock was poisoned",
                true,
            )
        })?;
        let root_task = persistence
            .load_delegated_task(cascade.root_task_id)?
            .ok_or_else(|| LoomError::not_found("delegated task", cascade.root_task_id))?;
        if root_task.project_id != cascade.project_id
            || root_task.requester_session_id != cascade.manager_session_id
        {
            return Err(LoomError::new(
                ErrorCode::RecoveryRequired,
                "persisted project cancellation identity no longer matches its task",
                false,
            ));
        }
        let workspace_id = self
            .backend
            .sessions()?
            .get(root_task.target_session_id)?
            .workspace_id;
        let workspace_admission = self.backend.admissions.workspace_project(workspace_id)?;
        let workspace_admission_guard = workspace_admission.lock().map_err(|_| {
            LoomError::new(
                ErrorCode::Internal,
                "workspace project scheduling lock was poisoned",
                true,
            )
        })?;
        let cancellation_sessions = cascade
            .members
            .iter()
            .map(|(_, session_id)| *session_id)
            .collect::<Vec<_>>();
        self.abandon_project_manager_waits_owned_by(persistence, &cancellation_sessions)?;

        // Terminalize members with no run before stopping active runs. Their
        // checkpoint callbacks may synchronously drain workspace admissions.
        for (task_id, session_id) in &cascade.members {
            let Some(mut task) = persistence.load_delegated_task(*task_id)? else {
                return Err(LoomError::new(
                    ErrorCode::RecoveryRequired,
                    "project cancellation member task is missing",
                    false,
                ));
            };
            if task.target_session_id != *session_id || task.project_id != cascade.project_id {
                return Err(LoomError::new(
                    ErrorCode::RecoveryRequired,
                    "project cancellation member no longer matches its saved snapshot",
                    false,
                ));
            }
            if !matches!(
                task.status,
                loom_core::DelegatedTaskStatus::Completed
                    | loom_core::DelegatedTaskStatus::Failed
                    | loom_core::DelegatedTaskStatus::Cancelled
            ) && persistence
                .load_latest_run_summary_for_session(*session_id)?
                .is_none()
            {
                self.set_project_task_status(
                    persistence,
                    &mut task,
                    loom_core::DelegatedTaskStatus::Cancelled,
                )?;
            }
        }
        drop(workspace_admission_guard);

        let mut root_run = None;
        for (task_id, session_id) in &cascade.members {
            let mut task = persistence
                .load_delegated_task(*task_id)?
                .ok_or_else(|| LoomError::not_found("delegated task", *task_id))?;
            let latest_run = persistence.load_latest_run_summary_for_session(*session_id)?;
            let mut run = latest_run
                .as_ref()
                .map(|summary| {
                    self.run_summary(summary.snapshot.id)
                        .map(|summary| summary.snapshot)
                })
                .transpose()?;
            if let Some(snapshot) = run.clone() {
                let terminal_status = match snapshot.state {
                    AgentRunState::Completed => Some(loom_core::DelegatedTaskStatus::Completed),
                    AgentRunState::Failed => Some(loom_core::DelegatedTaskStatus::Failed),
                    AgentRunState::Cancelled => Some(loom_core::DelegatedTaskStatus::Cancelled),
                    _ => None,
                };
                if let Some(status) = terminal_status {
                    self.set_project_task_status(persistence, &mut task, status)?;
                } else {
                    let response = self.stop_run(snapshot.id, RunStop::Interrupt)?;
                    let ServerResponse::Run(RunResponse::AgentRun(stopped)) = response else {
                        return Err(LoomError::new(
                            ErrorCode::Internal,
                            "project child cancel returned an unexpected response",
                            false,
                        ));
                    };
                    run = Some(stopped);
                    if run
                        .as_ref()
                        .is_some_and(|run| run.state == AgentRunState::Cancelled)
                    {
                        self.set_project_task_status(
                            persistence,
                            &mut task,
                            loom_core::DelegatedTaskStatus::Cancelled,
                        )?;
                    }
                }
            }
            if *task_id == cascade.root_task_id {
                root_run = run;
            }
        }
        self.backend.persist_state()?;
        if !persistence
            .complete_project_cancellation_cascade(cascade.project_id, cascade.root_task_id)?
        {
            return Err(LoomError::new(
                ErrorCode::RecoveryRequired,
                "project cancellation intent disappeared before completion",
                true,
            ));
        }
        let root_task = persistence
            .load_delegated_task(cascade.root_task_id)?
            .ok_or_else(|| LoomError::not_found("delegated task", cascade.root_task_id))?;
        Ok((root_task, root_run))
    }

    pub(crate) fn abandon_project_manager_waits_owned_by(
        &self,
        persistence: &dyn Persistence,
        sessions: &[AgentSessionId],
    ) -> Result<()> {
        let sessions = sessions.iter().copied().collect::<BTreeSet<_>>();
        for wait in persistence.list_unfinished_project_manager_waits()? {
            if sessions.contains(&wait.manager_session_id) {
                persistence.transition_project_manager_wait(
                    wait.wait_id,
                    wait.status,
                    loom_core::ProjectManagerWaitStatus::Abandoned,
                    None,
                    Timestamp::now(),
                )?;
            }
        }
        Ok(())
    }
}
