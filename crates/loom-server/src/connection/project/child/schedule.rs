use super::*;

impl InProcessConnection {
    pub(crate) fn drain_workspace_project_admissions(
        &self,
        workspace_id: WorkspaceId,
        recovering: bool,
    ) -> Result<()> {
        let admission = self.backend.admissions.workspace_project(workspace_id)?;
        let _admission = admission.lock().map_err(|_| {
            LoomError::new(
                ErrorCode::Internal,
                "workspace project scheduling lock was poisoned",
                true,
            )
        })?;
        self.drain_workspace_project_admissions_locked(workspace_id, recovering)
    }

    /// Runs while the workspace admission lock is held, selecting and admitting
    /// every currently eligible task and manager wait in one oldest-first pass.
    pub(crate) fn drain_workspace_project_admissions_locked(
        &self,
        workspace_id: WorkspaceId,
        recovering: bool,
    ) -> Result<()> {
        enum Candidate {
            ManagerWait(loom_core::ProjectManagerWaitRecord),
            DelegatedTask(loom_core::DelegatedTaskRecord),
        }

        let Some(persistence) = self.backend.persistence.as_ref() else {
            return Ok(());
        };
        let pending_cancellation_projects = persistence
            .list_pending_project_cancellation_cascades()?
            .into_iter()
            .map(|cascade| cascade.project_id)
            .collect::<BTreeSet<_>>();
        let mut candidates = self
            .workspace_project_tasks(workspace_id)?
            .into_iter()
            .filter(|task| {
                task.status == loom_core::DelegatedTaskStatus::Queued
                    && !pending_cancellation_projects.contains(&task.project_id)
            })
            .map(|task| {
                (
                    task.created_at,
                    task.task_id.to_string(),
                    Candidate::DelegatedTask(task),
                )
            })
            .collect::<Vec<_>>();
        for mut wait in persistence.list_unfinished_project_manager_waits()? {
            if self
                .backend
                .sessions()?
                .get(wait.manager_session_id)?
                .workspace_id
                != workspace_id
            {
                continue;
            }
            if persistence
                .load_project_snapshot_for_session(wait.manager_session_id)?
                .is_some_and(|project| pending_cancellation_projects.contains(&project.project_id))
            {
                continue;
            }
            if abandon_project_manager_wait_if_run_terminal(persistence, &wait)? {
                continue;
            }
            if wait.status == loom_core::ProjectManagerWaitStatus::Waiting
                && let Some(summary) = project_manager_wait_result_summary(persistence, &wait)?
                && persistence.transition_project_manager_wait(
                    wait.wait_id,
                    loom_core::ProjectManagerWaitStatus::Waiting,
                    loom_core::ProjectManagerWaitStatus::Ready,
                    Some(&summary),
                    Timestamp::now(),
                )?
            {
                wait.status = loom_core::ProjectManagerWaitStatus::Ready;
                wait.result_summary = Some(summary);
            }
            if !matches!(
                wait.status,
                loom_core::ProjectManagerWaitStatus::Ready
                    | loom_core::ProjectManagerWaitStatus::Resuming
            ) || (wait.status == loom_core::ProjectManagerWaitStatus::Resuming && !recovering)
            {
                continue;
            }
            candidates.push((
                wait.created_at,
                wait.wait_id.to_string(),
                Candidate::ManagerWait(wait),
            ));
        }
        candidates.sort_by(|left, right| (left.0, &left.1).cmp(&(right.0, &right.1)));
        for (_, _, candidate) in candidates {
            match candidate {
                Candidate::ManagerWait(wait) => {
                    self.resume_project_manager_wait_under_admission(&wait)?;
                }
                Candidate::DelegatedTask(mut task) => {
                    self.schedule_project_task_if_ready_under_admission(&mut task)?;
                }
            }
        }
        Ok(())
    }

    pub(crate) fn resume_project_manager_wait_under_admission(
        &self,
        wait: &loom_core::ProjectManagerWaitRecord,
    ) -> Result<()> {
        let persistence = self.backend.persistence.as_ref().ok_or_else(|| {
            LoomError::new(
                ErrorCode::UnsupportedCapability,
                "project manager waits require durable storage",
                false,
            )
        })?;
        let manager_session = self.backend.sessions()?.get(wait.manager_session_id)?;

        let Some(mut current_wait) = persistence.load_project_manager_wait(wait.wait_id)? else {
            return Ok(());
        };
        if abandon_project_manager_wait_if_run_terminal(persistence, &current_wait)? {
            return Ok(());
        }
        if !matches!(
            current_wait.status,
            loom_core::ProjectManagerWaitStatus::Ready
                | loom_core::ProjectManagerWaitStatus::Resuming
        ) {
            return Ok(());
        }
        let handle = self.run_handle(current_wait.run_id)?;
        let manager_run_state = handle.snapshot().state;
        if is_terminal_agent_run_state(manager_run_state) {
            persistence.transition_project_manager_wait(
                current_wait.wait_id,
                current_wait.status,
                loom_core::ProjectManagerWaitStatus::Abandoned,
                None,
                Timestamp::now(),
            )?;
            return Ok(());
        }
        if manager_run_state != AgentRunState::Paused {
            return Ok(());
        }
        if handle.is_running() {
            return Ok(());
        }

        let mut summary = current_wait.result_summary.clone();
        if summary.is_none() {
            summary = project_manager_wait_result_summary(persistence, &current_wait)?;
            if let Some(summary) = summary.as_deref() {
                current_wait.result_summary = Some(summary.to_owned());
            }
        }
        let Some(summary) = summary else {
            return Ok(());
        };

        let manager_task = persistence.load_delegated_task_for_target(wait.manager_session_id)?;
        let running_tasks = self
            .workspace_project_tasks(manager_session.workspace_id)?
            .iter()
            .filter(|task| {
                task.status == loom_core::DelegatedTaskStatus::Running
                    && manager_task
                        .as_ref()
                        .is_none_or(|manager_task| task.task_id != manager_task.task_id)
            })
            .count();
        let concurrency_limit = self
            .backend
            .workspace_configs()?
            .get(&manager_session.workspace_id)
            .map(|config| config.project_agent_concurrency)
            .unwrap_or_else(|| WorkspaceConfig::default().project_agent_concurrency);
        if !project_agent_capacity_available(running_tasks, concurrency_limit) {
            return Ok(());
        }

        if current_wait.status == loom_core::ProjectManagerWaitStatus::Ready {
            if !persistence.claim_project_manager_wait(current_wait.wait_id, Timestamp::now())? {
                return Ok(());
            }
            current_wait.status = loom_core::ProjectManagerWaitStatus::Resuming;
        }

        if let Some(task) = manager_task.as_ref()
            && task.status != loom_core::DelegatedTaskStatus::Running
            && !matches!(
                task.status,
                loom_core::DelegatedTaskStatus::Completed
                    | loom_core::DelegatedTaskStatus::Failed
                    | loom_core::DelegatedTaskStatus::Cancelled
            )
            && persistence.update_delegated_task_status(
                task.task_id,
                loom_core::DelegatedTaskStatus::Running,
                Timestamp::now(),
            )?
            && let Some(updated_task) = persistence.load_delegated_task(task.task_id)?
        {
            self.backend.journal()?.append_server_event(
                CURRENT_PROTOCOL_VERSION,
                updated_task.requester_session_id,
                ServerEvent::ProjectTaskUpdated { task: updated_task },
            );
        }

        let wait_id = current_wait.wait_id.to_string();
        let result = self.continue_run(current_wait.run_id, |runtime| {
            let continuation = runtime.pending_project_join();
            let call = continuation
                .as_ref()
                .map(|continuation| continuation.call.clone())
                .unwrap_or_else(|| loom_model::ToolCall {
                    id: current_wait.tool_call_id,
                    name: "wait_for_project_children".to_owned(),
                    arguments: serde_json::Value::Null,
                });
            if continuation
                .as_ref()
                .is_some_and(|continuation| continuation.wait_id != wait_id)
            {
                return Err(LoomError::new(
                    ErrorCode::RecoveryRequired,
                    "project manager wait does not match the persisted run continuation",
                    false,
                ));
            }
            runtime.complete_project_join(&wait_id, ToolResult::success(&call, summary.clone()))?;
            runtime.resume_entry()
        });
        if let Err(error) = result {
            if is_terminal_agent_run_state(handle.snapshot().state) {
                persistence.transition_project_manager_wait(
                    current_wait.wait_id,
                    loom_core::ProjectManagerWaitStatus::Resuming,
                    loom_core::ProjectManagerWaitStatus::Abandoned,
                    None,
                    Timestamp::now(),
                )?;
                return Ok(());
            }
            if let Some(task) = manager_task.as_ref()
                && persistence.update_delegated_task_status(
                    task.task_id,
                    loom_core::DelegatedTaskStatus::Blocked,
                    Timestamp::now(),
                )?
            {
                let Some(updated_task) = persistence.load_delegated_task(task.task_id)? else {
                    return Err(LoomError::not_found("delegated task", task.task_id));
                };
                self.backend.journal()?.append_server_event(
                    CURRENT_PROTOCOL_VERSION,
                    updated_task.requester_session_id,
                    ServerEvent::ProjectTaskUpdated { task: updated_task },
                );
            }
            return Err(error);
        }
        Ok(())
    }
}
