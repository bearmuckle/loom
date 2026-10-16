use super::*;

/// Outcome of one project child control action.
pub(crate) struct ProjectChildControlOutcome {
    pub(crate) task: loom_core::DelegatedTaskRecord,
    pub(crate) run: Option<AgentRunSnapshot>,
    /// Paths a finished code child left uncommitted when it was abandoned.
    pub(crate) uncommitted_paths: Vec<String>,
}

impl InProcessConnection {
    pub(crate) fn control_project_child(
        &self,
        manager_session_id: AgentSessionId,
        project_id: ProjectId,
        task_id: loom_core::TaskId,
        action: ProjectChildControlAction,
    ) -> Result<ProjectChildControlOutcome> {
        if !self.project_agent_permission_enabled_for_session(
            manager_session_id,
            Capability::ControlProjectChild,
            ProjectAgentPermission::ChildControl,
        )? {
            return Err(LoomError::new(
                ErrorCode::AuthorizationDenied,
                "project child control grant is no longer valid",
                false,
            ));
        }
        let persistence = self.backend.persistence.as_ref().ok_or_else(|| {
            LoomError::new(
                ErrorCode::UnsupportedCapability,
                "project child control requires durable storage",
                false,
            )
        })?;
        let project = self.load_project_snapshot(project_id)?;
        let mut task = persistence
            .load_delegated_task(task_id)?
            .ok_or_else(|| LoomError::not_found("delegated task", task_id))?;
        if task.project_id != project_id
            || task.requester_session_id != manager_session_id
            || !project.agents.iter().any(|agent| {
                agent.session_id == task.target_session_id
                    && agent.parent_session_id == Some(manager_session_id)
            })
        {
            return Err(LoomError::invalid_request(
                "task_id must identify one of this manager's direct child tasks",
            ));
        }
        if task.code_change
            && matches!(
                action,
                ProjectChildControlAction::Continue | ProjectChildControlAction::RetryFailedStep
            )
        {
            let mut worktree = persistence
                .load_project_worktree_by_task(task_id)?
                .ok_or_else(|| {
                    LoomError::new(
                        ErrorCode::RecoveryRequired,
                        "code task is missing its durable worktree record",
                        true,
                    )
                })?;
            self.ensure_project_worktree_ready(&mut worktree)?;
        }

        let latest_run = persistence.load_latest_run_summary_for_session(task.target_session_id)?;
        let mut run = latest_run
            .as_ref()
            .map(|summary| {
                self.run_summary(summary.snapshot.id)
                    .map(|summary| summary.snapshot)
            })
            .transpose()?;
        let mut uncommitted_paths = Vec::new();

        match action {
            ProjectChildControlAction::Continue => {
                if let Some(snapshot) = &run {
                    match snapshot.state {
                        AgentRunState::Paused => {
                            let response = self.resume_agent_run(snapshot.id)?;
                            let ServerResponse::Run(RunResponse::AgentRun(snapshot)) = response
                            else {
                                return Err(LoomError::new(
                                    ErrorCode::Internal,
                                    "project child resume returned an unexpected response",
                                    false,
                                ));
                            };
                            run = Some(snapshot);
                        }
                        AgentRunState::Planning
                        | AgentRunState::Executing
                        | AgentRunState::AwaitingApproval
                        | AgentRunState::Evaluating => {}
                        AgentRunState::NeedsInput => {
                            return Err(LoomError::new(
                                ErrorCode::InvalidState,
                                "the child is waiting for user input and cannot continue until it is answered",
                                false,
                            ));
                        }
                        AgentRunState::Completed
                        | AgentRunState::Failed
                        | AgentRunState::Cancelled => {
                            return Err(LoomError::new(
                                ErrorCode::InvalidState,
                                "the child run is finished; retry a failed tool step or create a new task",
                                false,
                            ));
                        }
                    }
                } else if matches!(
                    task.status,
                    loom_core::DelegatedTaskStatus::Queued
                        | loom_core::DelegatedTaskStatus::Blocked
                ) {
                    if task.status == loom_core::DelegatedTaskStatus::Blocked {
                        self.set_project_task_status(
                            persistence,
                            &mut task,
                            loom_core::DelegatedTaskStatus::Queued,
                        )?;
                    }
                    self.schedule_project_task_if_ready(&mut task)?;
                    run = persistence
                        .load_latest_run_summary_for_session(task.target_session_id)?
                        .map(|summary| self.run_summary(summary.snapshot.id))
                        .transpose()?
                        .map(|summary| summary.snapshot);
                } else {
                    return Err(LoomError::new(
                        ErrorCode::InvalidState,
                        "the delegated task has no resumable child run",
                        false,
                    ));
                }
            }
            ProjectChildControlAction::Pause => {
                let snapshot = run.as_ref().ok_or_else(|| {
                    LoomError::new(
                        ErrorCode::InvalidState,
                        "the delegated task has no child run to pause",
                        false,
                    )
                })?;
                match snapshot.state {
                    AgentRunState::Planning
                    | AgentRunState::Executing
                    | AgentRunState::AwaitingApproval
                    | AgentRunState::Evaluating => {
                        let response = self.stop_run(snapshot.id, RunStop::Pause)?;
                        let ServerResponse::Run(RunResponse::AgentRun(snapshot)) = response else {
                            return Err(LoomError::new(
                                ErrorCode::Internal,
                                "project child pause returned an unexpected response",
                                false,
                            ));
                        };
                        run = Some(snapshot);
                    }
                    AgentRunState::Paused => {}
                    AgentRunState::NeedsInput => {
                        return Err(LoomError::new(
                            ErrorCode::InvalidState,
                            "the child is waiting for user input and cannot be paused",
                            false,
                        ));
                    }
                    AgentRunState::Completed | AgentRunState::Failed | AgentRunState::Cancelled => {
                        return Err(LoomError::new(
                            ErrorCode::InvalidState,
                            "the child run is finished and cannot be paused",
                            false,
                        ));
                    }
                }
            }
            ProjectChildControlAction::Interrupt => {
                let snapshot = run.as_ref().ok_or_else(|| {
                    LoomError::new(
                        ErrorCode::InvalidState,
                        "the delegated task has no child run to interrupt",
                        false,
                    )
                })?;
                if matches!(
                    snapshot.state,
                    AgentRunState::Completed | AgentRunState::Failed | AgentRunState::Cancelled
                ) {
                    return Err(LoomError::new(
                        ErrorCode::InvalidState,
                        "the child run is already finished",
                        false,
                    ));
                }
                let response = self.stop_run(snapshot.id, RunStop::Interrupt)?;
                let ServerResponse::Run(RunResponse::AgentRun(snapshot)) = response else {
                    return Err(LoomError::new(
                        ErrorCode::Internal,
                        "project child interrupt returned an unexpected response",
                        false,
                    ));
                };
                run = Some(snapshot);
            }
            ProjectChildControlAction::RetryFailedStep => {
                let snapshot = run.as_ref().ok_or_else(|| {
                    LoomError::new(
                        ErrorCode::InvalidState,
                        "the delegated task has no failed child run",
                        false,
                    )
                })?;
                match snapshot.state {
                    AgentRunState::Failed => {
                        // One rule per failure case: a failure that recorded a
                        // failed tool step is retried at that step, while a
                        // failure that recorded none failed at the model or
                        // provider call (or a run limit), where there is no step
                        // to re-run and the run is resumed from its checkpoint
                        // instead. `continue` keeps refusing a finished run, so
                        // this is the only supported way to resume one; without it
                        // a provider failure would strand the work its checkout
                        // already holds until the manager abandons that work.
                        let has_failed_tool_step = persistence
                            .load_run_execution_state(snapshot.id)?
                            .and_then(|state| state.last_failed_call)
                            .is_some();
                        let workspace_id = self
                            .backend
                            .sessions()?
                            .get(task.target_session_id)?
                            .workspace_id;
                        let admission = self.backend.admissions.workspace_project(workspace_id)?;
                        let _admission = admission.lock().map_err(|_| {
                            LoomError::new(
                                ErrorCode::Internal,
                                "workspace project scheduling lock was poisoned",
                                true,
                            )
                        })?;
                        self.drain_workspace_project_admissions_locked(workspace_id, false)?;
                        let running_tasks = self
                            .workspace_project_tasks(workspace_id)?
                            .iter()
                            .filter(|candidate| {
                                candidate.task_id != task_id
                                    && candidate.status == loom_core::DelegatedTaskStatus::Running
                            })
                            .count();
                        let concurrency_limit = self
                            .backend
                            .workspace_configs()?
                            .get(&workspace_id)
                            .map(|config| config.project_agent_concurrency)
                            .unwrap_or_else(|| {
                                WorkspaceConfig::default().project_agent_concurrency
                            });
                        if !project_agent_capacity_available(running_tasks, concurrency_limit) {
                            return Err(LoomError::conflict(
                                "project agent concurrency limit reached; the child remains failed",
                            ));
                        }
                        let response = self.continue_run(
                            snapshot.id,
                            project_child_retry_entry(has_failed_tool_step),
                        )?;
                        let ServerResponse::Run(RunResponse::AgentRun(snapshot)) = response else {
                            return Err(LoomError::new(
                                ErrorCode::Internal,
                                "project child retry returned an unexpected response",
                                false,
                            ));
                        };
                        self.set_project_task_status(
                            persistence,
                            &mut task,
                            loom_core::DelegatedTaskStatus::Running,
                        )?;
                        run = Some(snapshot);
                    }
                    AgentRunState::Planning
                    | AgentRunState::Executing
                    | AgentRunState::AwaitingApproval
                    | AgentRunState::Evaluating => {}
                    AgentRunState::Paused
                    | AgentRunState::NeedsInput
                    | AgentRunState::Completed
                    | AgentRunState::Cancelled => {
                        return Err(LoomError::new(
                            ErrorCode::InvalidState,
                            "retry_failed_step requires a failed child run",
                            false,
                        ));
                    }
                }
            }
            ProjectChildControlAction::Cancel => {
                run = self.cancel_project_child_subtree(project_id, manager_session_id, task_id)?;
            }
            ProjectChildControlAction::Abandon => {
                uncommitted_paths = self.abandon_project_child_task(persistence, &mut task)?;
            }
        }
        task = persistence
            .load_delegated_task(task_id)?
            .ok_or_else(|| LoomError::not_found("delegated task", task_id))?;
        Ok(ProjectChildControlOutcome {
            task,
            run,
            uncommitted_paths,
        })
    }

    /// Closes a finished direct child task without landing its work.
    ///
    /// A code child that ends its run with uncommitted files has no committed
    /// revision to review or integrate, and every other control action refuses a
    /// finished run, so the manager would otherwise have no action that releases
    /// its completion guard. Abandoning records the live uncommitted paths on the
    /// worktree record, marks the task failed, and leaves the checkout in place
    /// for a later inspection or cleanup.
    ///
    /// The record and the note it writes say that this work will not be landed
    /// and will not be integrated, so a closed child is never mistaken for one
    /// whose result reached the parent.
    ///
    /// Returns the uncommitted paths it recorded. A task that is already closed
    /// is left as it is, so a retried request reports the same outcome.
    fn abandon_project_child_task(
        &self,
        persistence: &dyn Persistence,
        task: &mut loom_core::DelegatedTaskRecord,
    ) -> Result<Vec<String>> {
        let run = persistence.load_latest_run_summary_for_session(task.target_session_id)?;
        let run_is_live = run
            .as_ref()
            .is_some_and(|summary| !is_terminal_agent_run_state(summary.snapshot.state));
        if run_is_live
            || (run.is_none()
                && !matches!(
                    task.status,
                    loom_core::DelegatedTaskStatus::Completed
                        | loom_core::DelegatedTaskStatus::Failed
                        | loom_core::DelegatedTaskStatus::Cancelled
                ))
        {
            return Err(LoomError::new(
                ErrorCode::InvalidState,
                "the child is still live; cancel it or wait for it to finish before abandoning its result",
                false,
            ));
        }
        let mut uncommitted_paths = Vec::new();
        if task.code_change
            && let Some(mut worktree) = persistence.load_project_worktree_by_task(task.task_id)?
        {
            // The checkout may already be gone or unreadable, which must not keep
            // the manager from closing the task, so an inspection failure is
            // recorded in the outcome instead of raised.
            let note = match project_child_worktree_status(&self.backend, &worktree) {
                Ok(status) => {
                    uncommitted_paths = status
                        .files
                        .iter()
                        .map(|file| file.path.clone())
                        .collect::<Vec<_>>();
                    uncommitted_paths.sort();
                    uncommitted_paths.dedup();
                    if uncommitted_paths.is_empty() {
                        "the manager abandoned this child without landing its work, so nothing in its checkout will be integrated".to_owned()
                    } else {
                        format!(
                            "the manager abandoned this child without landing its work, so these uncommitted paths will not be integrated: {}",
                            uncommitted_paths.join(", ")
                        )
                    }
                }
                Err(error) => format!(
                    "the manager abandoned this child without landing its work; its checkout could not be inspected: {}",
                    error.message
                ),
            };
            worktree.error = Some(note);
            worktree.updated_at = Timestamp::now();
            self.save_project_worktree_state(&worktree)?;
        }
        self.set_project_task_status(persistence, task, loom_core::DelegatedTaskStatus::Failed)?;
        Ok(uncommitted_paths)
    }

    /// Cancels one direct child task and its descendants, returning the
    /// cancelled task's latest run.
    ///
    /// The cancellation intent is persisted for the captured subtree, every
    /// live run in it is interrupted, its tasks are terminalized, and the
    /// manager waits it owns are abandoned. Callers must not hold the project
    /// admission lock: it is acquired here and the hierarchy is refreshed
    /// afterwards, so a descendant committed while this waited is included in
    /// the subtree walk.
    pub(crate) fn cancel_project_child_subtree(
        &self,
        project_id: ProjectId,
        manager_session_id: AgentSessionId,
        task_id: loom_core::TaskId,
    ) -> Result<Option<AgentRunSnapshot>> {
        let persistence = self.backend.persistence.as_ref().ok_or_else(|| {
            LoomError::new(
                ErrorCode::UnsupportedCapability,
                "project child cancellation requires durable storage",
                false,
            )
        })?;
        let admission = self.backend.admissions.project(project_id)?;
        let _admission_guard = admission.lock().map_err(|_| {
            LoomError::new(
                ErrorCode::Internal,
                "project scheduling lock was poisoned",
                true,
            )
        })?;
        // Child creation uses this same lock. Refresh the hierarchy after
        // acquiring it so a descendant committed while this cancellation was
        // waiting is included in the subtree walk.
        let project = self.load_project_snapshot(project_id)?;
        let task = persistence
            .load_delegated_task(task_id)?
            .ok_or_else(|| LoomError::not_found("delegated task", task_id))?;
        let mut run = persistence
            .load_latest_run_summary_for_session(task.target_session_id)?
            .as_ref()
            .map(|summary| {
                self.run_summary(summary.snapshot.id)
                    .map(|summary| summary.snapshot)
            })
            .transpose()?;
        if let Some(snapshot) = &run {
            if snapshot.state == AgentRunState::Completed {
                return Err(LoomError::new(
                    ErrorCode::InvalidState,
                    "the child run is already completed",
                    false,
                ));
            }
        } else if !matches!(
            task.status,
            loom_core::DelegatedTaskStatus::Queued
                | loom_core::DelegatedTaskStatus::Blocked
                | loom_core::DelegatedTaskStatus::Failed
                | loom_core::DelegatedTaskStatus::Cancelled
        ) {
            return Err(LoomError::new(
                ErrorCode::InvalidState,
                "the delegated task has no cancellable child run",
                false,
            ));
        }

        // Use the persisted hierarchy rather than depth alone: older or
        // repaired snapshots may not be ordered by depth. Post-order
        // traversal also guards against malformed cycles and duplicates.
        let cancellation_order = project_subtree_deepest_first(&project, task.target_session_id);
        let cascade = ProjectCancellationCascadeRecord {
            project_id,
            root_task_id: task.task_id,
            manager_session_id,
            members: cancellation_order
                .iter()
                .map(|session_id| {
                    persistence
                        .load_delegated_task_for_target(*session_id)?
                        .map(|task| (task.task_id, *session_id))
                        .ok_or_else(|| {
                            LoomError::new(
                                ErrorCode::RecoveryRequired,
                                "project cancellation member has no delegated task",
                                false,
                            )
                        })
                })
                .collect::<Result<Vec<_>>>()?,
            created_at: Timestamp::now(),
        };
        let cascade = persistence.begin_project_cancellation_cascade(&cascade)?;
        #[cfg(test)]
        if self
            .backend
            .project_cancellation_failpoint
            .compare_exchange(usize::MAX, 0, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
        {
            return Err(LoomError::new(
                ErrorCode::RecoveryRequired,
                "test interruption after persisting project cancellation intent",
                true,
            ));
        }
        let workspace_id = self
            .backend
            .sessions()?
            .get(task.target_session_id)?
            .workspace_id;
        let workspace_admission = self.backend.admissions.workspace_project(workspace_id)?;
        let workspace_admission_guard = workspace_admission.lock().map_err(|_| {
            LoomError::new(
                ErrorCode::Internal,
                "workspace project scheduling lock was poisoned",
                true,
            )
        })?;
        self.abandon_project_manager_waits_owned_by(persistence, &cancellation_order)?;

        // Prevent queued descendants from being started by the task
        // reconciler triggered as active runs are interrupted.
        for session_id in &cancellation_order {
            let Some(mut queued_task) = persistence.load_delegated_task_for_target(*session_id)?
            else {
                continue;
            };
            if !matches!(
                queued_task.status,
                loom_core::DelegatedTaskStatus::Completed
                    | loom_core::DelegatedTaskStatus::Failed
                    | loom_core::DelegatedTaskStatus::Cancelled
            ) && persistence
                .load_latest_run_summary_for_session(*session_id)?
                .is_none()
            {
                self.set_project_task_status(
                    persistence,
                    &mut queued_task,
                    loom_core::DelegatedTaskStatus::Cancelled,
                )?;
            }
        }
        drop(workspace_admission_guard);

        // Keep project admission locked until every run in the captured
        // subtree has stopped. This prevents a manager from creating a
        // new descendant after the subtree snapshot. The workspace
        // admission lock was released above because stop_run reconciles
        // queued tasks and waits synchronously.

        for session_id in cancellation_order {
            let is_selected_child = session_id == task.target_session_id;
            let mut descendant_task = persistence.load_delegated_task_for_target(session_id)?;
            let latest_run = persistence.load_latest_run_summary_for_session(session_id)?;
            let mut descendant_run = latest_run
                .as_ref()
                .map(|summary| {
                    self.run_summary(summary.snapshot.id)
                        .map(|summary| summary.snapshot)
                })
                .transpose()?;

            if let Some(snapshot) = descendant_run.clone() {
                match snapshot.state {
                    AgentRunState::Cancelled => {
                        if let Some(descendant_task) = descendant_task.as_mut()
                            && descendant_task.status != loom_core::DelegatedTaskStatus::Cancelled
                        {
                            self.set_project_task_status(
                                persistence,
                                descendant_task,
                                loom_core::DelegatedTaskStatus::Cancelled,
                            )?;
                        }
                    }
                    AgentRunState::Completed | AgentRunState::Failed => {
                        if is_selected_child && snapshot.state == AgentRunState::Completed {
                            return Err(LoomError::new(
                                ErrorCode::InvalidState,
                                "the child run is already completed",
                                false,
                            ));
                        }
                        if let Some(descendant_task) = descendant_task.as_mut() {
                            let status = match snapshot.state {
                                AgentRunState::Completed => {
                                    loom_core::DelegatedTaskStatus::Completed
                                }
                                AgentRunState::Failed => loom_core::DelegatedTaskStatus::Failed,
                                _ => unreachable!("terminal state matched above"),
                            };
                            if descendant_task.status != status {
                                self.set_project_task_status(persistence, descendant_task, status)?;
                            }
                        }
                    }
                    _ => {
                        let response = self.stop_run(snapshot.id, RunStop::Interrupt)?;
                        let ServerResponse::Run(RunResponse::AgentRun(stopped)) = response else {
                            return Err(LoomError::new(
                                ErrorCode::Internal,
                                "project child cancel returned an unexpected response",
                                false,
                            ));
                        };
                        descendant_run = Some(stopped);
                        if let Some(descendant_task) = descendant_task.as_mut()
                            && let Some(run) = descendant_run.as_ref()
                            && is_terminal_agent_run_state(run.state)
                        {
                            self.set_project_task_status(
                                persistence,
                                descendant_task,
                                delegated_task_status_for_run_state(run.state),
                            )?;
                        }
                    }
                }
            } else if let Some(descendant_task) = descendant_task.as_mut()
                && !matches!(
                    descendant_task.status,
                    loom_core::DelegatedTaskStatus::Completed
                        | loom_core::DelegatedTaskStatus::Failed
                        | loom_core::DelegatedTaskStatus::Cancelled
                )
            {
                // Queued, blocked, or stale running records without a
                // run have no worker to interrupt and can be terminalized
                // directly.
                self.set_project_task_status(
                    persistence,
                    descendant_task,
                    loom_core::DelegatedTaskStatus::Cancelled,
                )?;
            }

            if is_selected_child {
                run = descendant_run;
            }

            #[cfg(test)]
            if consume_project_cancellation_failpoint(&self.backend.project_cancellation_failpoint)
            {
                return Err(LoomError::new(
                    ErrorCode::RecoveryRequired,
                    "test interruption after a durable project cancellation member update",
                    true,
                ));
            }
        }

        // Queued direct children can become terminal without a run
        // checkpoint, so reconcile joins and newly unblocked work once
        // the complete subtree has been updated.
        self.backend
            .reconcile_project_tasks_and_resume_queued(false)?;
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
        // The first reconciliation was fenced by the pending intent so
        // callbacks could not admit work from this project mid-cascade.
        self.backend
            .reconcile_project_tasks_and_resume_queued(false)?;
        Ok(run)
    }

    /// Cancels the live work of `project` before the deletion path removes it.
    ///
    /// The deletion path uses this so a project with live sub-agents can be
    /// deleted immediately instead of waiting for its tree to finish. A project
    /// whose top-level tasks are all terminal and whose root session has no
    /// loaded live run has no live work and needs nothing here.
    ///
    /// The manager's own run of the root session is interrupted first, because a
    /// manager that is running or parked in `wait_for_project_children` owns a
    /// non-terminal run that the backend deletion guard refuses. Stopping it
    /// first also stops it from committing new children while the tree is
    /// cancelled. The project admission lock is deliberately not held across
    /// that call: `stop_run` reconciles the project and takes the same lock
    /// itself.
    ///
    /// The project snapshot is re-read afterwards, so a child the manager
    /// committed before it stopped is still cancelled, and each non-terminal
    /// top-level task subtree is cancelled through `cancel_project_child_subtree`,
    /// which takes the project admission lock itself and refreshes the hierarchy
    /// under it. A task created after that last reload is refused by the backend
    /// task guard; the race is retryable, and a retried request observes and
    /// cancels the newer task.
    pub(crate) fn cancel_project_live_work(&self, project: &ProjectSnapshot) -> Result<()> {
        self.interrupt_project_root_runs(project.root_session_id)?;
        let project = self.load_project_snapshot(project.project_id)?;
        let live_tasks = project
            .tasks
            .iter()
            .filter(|task| task.requester_session_id == project.root_session_id)
            .filter(|task| {
                !matches!(
                    task.status,
                    loom_core::DelegatedTaskStatus::Completed
                        | loom_core::DelegatedTaskStatus::Failed
                        | loom_core::DelegatedTaskStatus::Cancelled
                )
            })
            .map(|task| task.task_id)
            .collect::<Vec<_>>();
        if live_tasks.is_empty() {
            return Ok(());
        }
        log::info!(
            "[loom-server] cancelling {} live top-level task(s) of project {} before its deletion",
            live_tasks.len(),
            project.project_id
        );
        for task_id in live_tasks {
            self.cancel_project_child_subtree(
                project.project_id,
                project.root_session_id,
                task_id,
            )?;
        }
        Ok(())
    }

    /// Interrupts every non-terminal run that this process has loaded for
    /// `session_id`, which is the project manager's own run for a project root.
    ///
    /// A persisted run summary without a loaded handle is left alone: it has no
    /// executor that could keep working, so it cannot commit new children and
    /// the deletion guard ignores it.
    fn interrupt_project_root_runs(&self, session_id: AgentSessionId) -> Result<()> {
        let run_ids = self
            .backend
            .runs()?
            .iter()
            .filter(|(_, handle)| handle.session_id == session_id)
            .filter(|(_, handle)| !is_terminal_agent_run_state(handle.snapshot().state))
            .map(|(run_id, _)| *run_id)
            .collect::<Vec<_>>();
        for run_id in run_ids {
            log::info!(
                "[loom-server] interrupting run {run_id} of project root {session_id} before its deletion"
            );
            self.stop_run(run_id, RunStop::Interrupt)?;
        }
        Ok(())
    }
}

/// The runtime entry a failed child run is retried through.
type ProjectChildRetryEntry = fn(&mut AgentRuntime) -> Result<RunProgress>;

/// Chooses the retry entry for one failed child run, one rule per failure case.
///
/// A failure that recorded a failed tool step is retried at that step, because
/// re-running it is exactly the work that failed. A failure that recorded no
/// tool step failed at the model or provider call (or at a run limit) instead,
/// where there is no step to re-run: that run is resumed from its checkpoint, so
/// the child can finish its task and commit the work its checkout already holds.
/// Refusing a finished run stays the rule for `continue`; the point of this
/// choice is that a finished run's written work remains reachable through the
/// surface, because its worktree cannot be reviewed or integrated while nothing
/// in it is committed.
fn project_child_retry_entry(has_failed_tool_step: bool) -> ProjectChildRetryEntry {
    if has_failed_tool_step {
        AgentRuntime::retry_entry
    } else {
        AgentRuntime::checkpoint_retry_entry
    }
}

/// Atomically decrements a positive test failpoint counter, returning whether
/// the decrement consumed the last count.
///
/// A compare-exchange loop rather than `Atomic::fetch_update`, which is
/// deprecated on newer toolchains while its replacement is not yet available on
/// the workspace's minimum supported Rust version.
#[cfg(test)]
fn consume_project_cancellation_failpoint(failpoint: &AtomicUsize) -> bool {
    let mut remaining = failpoint.load(Ordering::SeqCst);
    while remaining > 0 {
        match failpoint.compare_exchange_weak(
            remaining,
            remaining - 1,
            Ordering::SeqCst,
            Ordering::SeqCst,
        ) {
            Ok(_) => return remaining == 1,
            Err(actual) => remaining = actual,
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::{consume_project_cancellation_failpoint, project_child_retry_entry};
    use crate::{AgentRuntime, RunProgress};
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// The chosen entry of a failure case, comparable across the two function
    /// items because both share one signature.
    fn entry_address(entry: fn(&mut AgentRuntime) -> loom_core::Result<RunProgress>) -> usize {
        entry as usize
    }

    #[test]
    fn a_failed_step_is_retried_and_a_provider_failure_resumes_from_its_checkpoint() {
        assert_eq!(
            entry_address(project_child_retry_entry(true)),
            entry_address(AgentRuntime::retry_entry),
            "a recorded failed tool step is retried at that step"
        );
        assert_eq!(
            entry_address(project_child_retry_entry(false)),
            entry_address(AgentRuntime::checkpoint_retry_entry),
            "a failure with no failed tool step resumes the run from its checkpoint"
        );
    }

    #[test]
    fn failpoint_consumption_reports_the_last_count() {
        let failpoint = AtomicUsize::new(2);
        assert!(!consume_project_cancellation_failpoint(&failpoint));
        assert!(consume_project_cancellation_failpoint(&failpoint));
        // Exhausted counters do not underflow and do not trigger.
        assert!(!consume_project_cancellation_failpoint(&failpoint));
        assert_eq!(failpoint.load(Ordering::SeqCst), 0);
    }
}
