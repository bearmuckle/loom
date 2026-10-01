use super::*;

impl InProcessConnection {
    pub(crate) fn control_project_child(
        &self,
        manager_session_id: AgentSessionId,
        project_id: ProjectId,
        task_id: loom_core::TaskId,
        action: ProjectChildControlAction,
    ) -> Result<(loom_core::DelegatedTaskRecord, Option<AgentRunSnapshot>)> {
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
                        let response = self.continue_run(snapshot.id, AgentRuntime::retry_entry)?;
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
                            "retry_failed_step requires a failed run with a retryable tool step",
                            false,
                        ));
                    }
                }
            }
            ProjectChildControlAction::Cancel => {
                let admission = self.backend.admissions.project(project_id)?;
                let admission_guard = admission.lock().map_err(|_| {
                    LoomError::new(
                        ErrorCode::Internal,
                        "project scheduling lock was poisoned",
                        true,
                    )
                })?;
                // Child creation uses this same lock. Refresh the hierarchy
                // after acquiring it so a descendant committed while this
                // cancellation was waiting is included in the subtree walk.
                let project = self.load_project_snapshot(project_id)?;
                task = persistence
                    .load_delegated_task(task_id)?
                    .ok_or_else(|| LoomError::not_found("delegated task", task_id))?;
                run = persistence
                    .load_latest_run_summary_for_session(task.target_session_id)?
                    .as_ref()
                    .map(|summary| {
                        self.run_summary(summary.snapshot.id)
                            .map(|summary| summary.snapshot)
                    })
                    .transpose()?;
                if let Some(snapshot) = &run {
                    if snapshot.state == AgentRunState::Completed {
                        drop(admission_guard);
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
                    drop(admission_guard);
                    return Err(LoomError::new(
                        ErrorCode::InvalidState,
                        "the delegated task has no cancellable child run",
                        false,
                    ));
                }

                // Use the persisted hierarchy rather than depth alone: older or
                // repaired snapshots may not be ordered by depth. Post-order
                // traversal also guards against malformed cycles and duplicates.
                let cancellation_order =
                    project_subtree_deepest_first(&project, task.target_session_id);
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
                let workspace_admission =
                    self.backend.admissions.workspace_project(workspace_id)?;
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
                    let Some(mut queued_task) =
                        persistence.load_delegated_task_for_target(*session_id)?
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
                    let mut descendant_task =
                        persistence.load_delegated_task_for_target(session_id)?;
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
                                    && descendant_task.status
                                        != loom_core::DelegatedTaskStatus::Cancelled
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
                                        AgentRunState::Failed => {
                                            loom_core::DelegatedTaskStatus::Failed
                                        }
                                        _ => unreachable!("terminal state matched above"),
                                    };
                                    if descendant_task.status != status {
                                        self.set_project_task_status(
                                            persistence,
                                            descendant_task,
                                            status,
                                        )?;
                                    }
                                }
                            }
                            _ => {
                                let response = self.stop_run(snapshot.id, RunStop::Interrupt)?;
                                let ServerResponse::Run(RunResponse::AgentRun(stopped)) = response
                                else {
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
                        task = persistence
                            .load_delegated_task(task_id)?
                            .ok_or_else(|| LoomError::not_found("delegated task", task_id))?;
                        run = descendant_run;
                    }

                    #[cfg(test)]
                    if consume_project_cancellation_failpoint(
                        &self.backend.project_cancellation_failpoint,
                    ) {
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
                if !persistence.complete_project_cancellation_cascade(
                    cascade.project_id,
                    cascade.root_task_id,
                )? {
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
            }
        }
        task = persistence
            .load_delegated_task(task_id)?
            .ok_or_else(|| LoomError::not_found("delegated task", task_id))?;
        Ok((task, run))
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
    use super::consume_project_cancellation_failpoint;
    use std::sync::atomic::{AtomicUsize, Ordering};

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
