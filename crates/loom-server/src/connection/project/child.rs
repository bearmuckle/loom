use super::*;

impl InProcessConnection {
    pub(crate) fn create_project_child(
        &self,
        request_id: RequestId,
        parent_session_id: AgentSessionId,
        child_name: String,
        spec: loom_core::DelegatedTaskSpec,
    ) -> Result<ServerResponse> {
        let permissions = spec.permissions;
        if spec.code_change
            && !self
                .backend
                .supported_capabilities
                .contains(Capability::CreateProjectWorktree)
        {
            return Err(LoomError::new(
                ErrorCode::UnsupportedCapability,
                "project worktree support is unavailable",
                false,
            ));
        }
        if child_name.trim().is_empty() || child_name.len() > 128 {
            return Err(LoomError::invalid_request(
                "child name must contain 1 to 128 bytes",
            ));
        }
        if spec.model_id.trim().is_empty() || spec.model_id.len() > 512 {
            return Err(LoomError::invalid_request(
                "delegated task model ID must contain 1 to 512 bytes",
            ));
        }
        if self.backend.persistence.is_none() {
            return Err(LoomError::new(
                ErrorCode::UnsupportedCapability,
                "delegated child creation requires durable storage",
                false,
            ));
        }
        let model_id = ModelId::new(spec.model_id.clone());
        self.backend.provider(&model_id)?;
        self.backend.providers.pricing(&model_id)?;
        if spec.intent.trim().is_empty() || spec.intent.len() > 16 * 1024 {
            return Err(LoomError::invalid_request(
                "delegated task intent must contain 1 to 16384 bytes",
            ));
        }
        if spec.context_references.len() > 128 || spec.dependencies.len() > 128 {
            return Err(LoomError::invalid_request(
                "delegated task references and dependencies are limited to 128 each",
            ));
        }
        let project = self.load_project_snapshot_for_session(parent_session_id)?;
        let project_id = project.project_id;
        let parent = project
            .agents
            .iter()
            .find(|agent| agent.session_id == parent_session_id)
            .ok_or_else(|| LoomError::invalid_request("requester is not a project member"))?;
        if parent.depth >= MAX_PROJECT_AGENT_DEPTH {
            return Err(LoomError::invalid_request(
                "project agent hierarchy exceeds maximum depth",
            ));
        }
        let requested_permissions = [
            (
                permissions.delegation,
                Capability::CreateProjectChild,
                ProjectAgentPermission::Delegation,
                "delegation",
            ),
            (
                permissions.branch_messaging,
                Capability::SendProjectBranchMessage,
                ProjectAgentPermission::BranchMessaging,
                "branch messaging",
            ),
            (
                permissions.child_control,
                Capability::ControlProjectChild,
                ProjectAgentPermission::ChildControl,
                "child control",
            ),
            (
                permissions.inspection,
                Capability::ReadProject,
                ProjectAgentPermission::Inspection,
                "project inspection",
            ),
            (
                permissions.worktree_creation,
                Capability::CreateProjectWorktree,
                ProjectAgentPermission::WorktreeCreation,
                "worktree creation",
            ),
            (
                permissions.review,
                Capability::ReadProjectChildReview,
                ProjectAgentPermission::Review,
                "child review",
            ),
            (
                permissions.integration,
                Capability::IntegrateProjectChild,
                ProjectAgentPermission::Integration,
                "child integration",
            ),
        ];
        for (requested, capability, permission, label) in requested_permissions {
            if requested
                && !self.project_agent_permission_enabled_for_session(
                    parent_session_id,
                    capability,
                    permission,
                )?
            {
                return Err(LoomError::new(
                    ErrorCode::AuthorizationDenied,
                    format!("parent is not authorized to grant {label} to a child"),
                    false,
                ));
            }
        }
        if !self.project_agent_permission_enabled_for_session(
            parent_session_id,
            Capability::CreateProjectChild,
            ProjectAgentPermission::Delegation,
        )? {
            return Err(LoomError::new(
                ErrorCode::AuthorizationDenied,
                "project delegation grant is no longer valid",
                false,
            ));
        }
        if spec.code_change
            && !self.project_agent_permission_enabled_for_session(
                parent_session_id,
                Capability::CreateProjectWorktree,
                ProjectAgentPermission::WorktreeCreation,
            )?
        {
            return Err(LoomError::new(
                ErrorCode::AuthorizationDenied,
                "project code delegation grant is no longer valid",
                false,
            ));
        }
        if let Some(auth) = &self.auth {
            if !auth.scope().allows_session(parent_session_id) {
                return Err(unauthorized_session(parent_session_id));
            }
            if !auth.scope().allows_workspace(
                self.backend
                    .sessions()?
                    .get(parent_session_id)?
                    .workspace_id,
            ) {
                return Err(LoomError::new(
                    ErrorCode::AuthorizationDenied,
                    "token is not authorized for the project's workspace",
                    false,
                ));
            }
        }
        let persistence = self.backend.persistence.as_ref().ok_or_else(|| {
            LoomError::new(
                ErrorCode::UnsupportedCapability,
                "delegated child creation requires durable storage",
                false,
            )
        })?;
        if let Some(existing_task) = persistence.load_project_child_by_request(
            request_id,
            project_id,
            parent_session_id,
            &child_name,
            &spec,
        )? {
            let mut existing_project = project;
            if !existing_project
                .agents
                .iter()
                .any(|agent| agent.session_id == existing_task.target_session_id)
            {
                let state = persistence.load_sessions()?.ok_or_else(|| {
                    LoomError::new(
                        ErrorCode::Persistence,
                        "persisted child session is missing from the session catalog",
                        false,
                    )
                })?;
                *self.backend.sessions()? = SessionManager::from_state(state)?;
                existing_project = self.load_project_snapshot(project_id)?;
            }
            let child = existing_project
                .agents
                .iter()
                .find(|agent| agent.session_id == existing_task.target_session_id)
                .cloned()
                .ok_or_else(|| {
                    LoomError::new(
                        ErrorCode::Persistence,
                        "delegated task child is missing from its project hierarchy",
                        false,
                    )
                })?;
            if !self
                .backend
                .session_filesystems()?
                .contains_key(&existing_task.target_session_id)
            {
                let filesystem = if self
                    .backend
                    .persisted_session_filesystems()?
                    .contains(&existing_task.target_session_id)
                {
                    self.session_filesystem(existing_task.target_session_id)?
                } else {
                    self.backend.create_session_filesystem(
                        self.backend
                            .sessions()?
                            .get(existing_task.target_session_id)?
                            .workspace_id,
                        existing_task.target_session_id,
                    )?
                };
                self.backend
                    .session_filesystems()?
                    .insert(existing_task.target_session_id, filesystem);
            }
            let mut existing_task = existing_task;
            if existing_task.code_change {
                let mut worktree = persistence
                    .load_project_worktree_by_task(existing_task.task_id)?
                    .ok_or_else(|| {
                        LoomError::new(
                            ErrorCode::RecoveryRequired,
                            "code task is missing its durable worktree intent",
                            true,
                        )
                    })?;
                self.ensure_project_worktree_ready(&mut worktree)?;
            }
            self.schedule_project_task_if_ready(&mut existing_task)?;
            return Ok(ServerResponse::ProjectChildCreated {
                task: existing_task,
                child,
            });
        }
        let admission = self.backend.admissions.project(project_id)?;
        let admission_guard = admission.lock().map_err(|_| {
            LoomError::new(
                ErrorCode::Internal,
                "project scheduling lock was poisoned",
                true,
            )
        })?;
        if persistence.has_pending_project_cancellation_cascade(project_id)? {
            return Err(LoomError::new(
                ErrorCode::RecoveryRequired,
                "project child creation is paused until its pending cancellation cascade is recovered",
                true,
            ));
        }
        let parent_snapshot = self.backend.sessions()?.get(parent_session_id)?;
        let workspace_admission = self
            .backend
            .admissions
            .workspace_project(parent_snapshot.workspace_id)?;
        let workspace_admission_guard = workspace_admission.lock().map_err(|_| {
            LoomError::new(
                ErrorCode::Internal,
                "workspace project scheduling lock was poisoned",
                true,
            )
        })?;
        let state_persist_guard = self.backend.state_persist_gate.lock().map_err(|_| {
            LoomError::new(
                ErrorCode::Internal,
                "state persistence lock was poisoned",
                true,
            )
        })?;
        let current_tasks = persistence.list_project_tasks(project_id)?;
        if spec
            .dependencies
            .iter()
            .any(|dependency| !current_tasks.iter().any(|task| task.task_id == *dependency))
        {
            return Err(LoomError::invalid_request(
                "delegated task dependencies must reference tasks in the same project",
            ));
        }
        let nonterminal_tasks = current_tasks
            .iter()
            .filter(|task| {
                !matches!(
                    task.status,
                    loom_core::DelegatedTaskStatus::Completed
                        | loom_core::DelegatedTaskStatus::Failed
                        | loom_core::DelegatedTaskStatus::Cancelled
                )
            })
            .count();
        if nonterminal_tasks >= MAX_NONTERMINAL_PROJECT_TASKS {
            return Err(LoomError::conflict(format!(
                "project already has {MAX_NONTERMINAL_PROJECT_TASKS} queued or active tasks"
            )));
        }
        let child_session_id = AgentSessionId::new();
        let mut timestamp = loom_core::Timestamp::now();
        let mut child_snapshot = AgentSessionSnapshot {
            id: child_session_id,
            workspace_id: parent_snapshot.workspace_id,
            name: child_name.clone(),
            state: loom_core::AgentSessionState::Idle,
            created_at: timestamp,
            updated_at: timestamp,
        };
        let task_id = loom_core::TaskId::new();
        let mut task = loom_core::DelegatedTaskRecord {
            task_id,
            project_id,
            requester_session_id: parent_session_id,
            target_session_id: child_session_id,
            child_name: child_name.clone(),
            intent: spec.intent,
            model_id: model_id.as_str().to_owned(),
            context_references: spec.context_references,
            dependencies: spec.dependencies,
            code_change: spec.code_change,
            permissions,
            status: loom_core::DelegatedTaskStatus::Queued,
            created_at: timestamp,
            updated_at: timestamp,
        };
        let mut initial_worktree = None;
        let mut precreated_child_filesystem = None;
        if task.code_change {
            let repositories = self
                .backend
                .session_repositories()?
                .get(&parent_session_id)
                .cloned()
                .unwrap_or_default();
            if repositories.len() != 1 {
                return Err(LoomError::invalid_request(
                    "code tasks currently require exactly one Git repository attached to the parent session",
                ));
            }
            let (parent_repository_id, _) = repositories
                .into_iter()
                .next()
                .expect("repository count checked above");
            let parent_git = self.session_git(parent_session_id, parent_repository_id)?;
            let parent_status = parent_git.status()?;
            if !parent_status.clean || parent_status.branch.is_none() {
                return Err(LoomError::conflict(
                    "code tasks require a clean parent checkout on a local branch",
                ));
            }
            let base_revision = parent_status.head.ok_or_else(|| {
                LoomError::invalid_state("parent repository HEAD does not point to a commit")
            })?;
            let child_repository_id = RepositoryId::new();
            let child_filesystem = self
                .backend
                .create_session_filesystem(parent_snapshot.workspace_id, child_session_id)?;
            let worktree_relative_path = format!("project-worktrees/{task_id}");
            initial_worktree = Some(ProjectWorktreeRecord {
                project_id,
                task_id,
                parent_session_id,
                child_session_id,
                parent_repository_id,
                child_repository_id,
                relative_path: worktree_relative_path,
                worktree_name: format!("loom-child-{task_id}"),
                branch_name: format!("loom/project-child-{task_id}"),
                base_revision,
                result_revision: None,
                integrated_revision: None,
                status: ProjectWorktreeStatus::Creating,
                conflict_paths: Vec::new(),
                error: None,
                cleanup_disposition: None,
                created_at: timestamp,
                updated_at: timestamp,
            });
            precreated_child_filesystem = Some(child_filesystem);
        }
        timestamp = loom_core::Timestamp::now();
        child_snapshot.created_at = timestamp;
        child_snapshot.updated_at = timestamp;
        task.created_at = timestamp;
        task.updated_at = timestamp;
        if let Some(worktree) = initial_worktree.as_mut() {
            worktree.created_at = timestamp;
            worktree.updated_at = timestamp;
        }
        let next_sequence = self.backend.sessions()?.next_sequence().next();
        let create_result = match initial_worktree.as_ref() {
            Some(worktree) => persistence.create_project_child_with_worktree(
                request_id,
                &child_snapshot,
                next_sequence,
                &task,
                worktree,
            ),
            None => {
                persistence.create_project_child(request_id, &child_snapshot, next_sequence, &task)
            }
        };
        let mut persisted_task = match create_result {
            Ok(task) => task,
            Err(error) => {
                if let Some(filesystem) = precreated_child_filesystem {
                    let _ = fs::remove_dir_all(filesystem.root());
                }
                return Err(error);
            }
        };
        let actual_child_session_id = persisted_task.target_session_id;
        let was_created = actual_child_session_id == child_session_id;
        let actual_snapshot = match self.backend.sessions()?.get(actual_child_session_id) {
            Ok(snapshot) => snapshot,
            Err(_) if !was_created => {
                let persisted_sessions = persistence.load_sessions()?.ok_or_else(|| {
                    LoomError::new(
                        ErrorCode::Persistence,
                        "persisted child session is missing from the session catalog",
                        false,
                    )
                })?;
                let snapshot = persisted_sessions
                    .sessions
                    .get(&actual_child_session_id)
                    .cloned()
                    .ok_or_else(|| {
                        LoomError::not_found("agent session", actual_child_session_id)
                    })?;
                *self.backend.sessions()? = SessionManager::from_state(persisted_sessions)?;
                snapshot
            }
            Err(_) => child_snapshot.clone(),
        };
        if was_created {
            let (created, event) = self.backend.sessions()?.create_in_workspace_with_id(
                actual_snapshot.workspace_id,
                actual_child_session_id,
                child_name,
            )?;
            self.backend.journal()?.append_session(event);
            let filesystem = match precreated_child_filesystem.take() {
                Some(filesystem) => filesystem,
                None => self
                    .backend
                    .create_session_filesystem(created.workspace_id, actual_child_session_id)?,
            };
            self.backend
                .session_filesystems()?
                .insert(actual_child_session_id, filesystem);
            self.backend
                .session_repositories()?
                .insert(actual_child_session_id, BTreeMap::new());
        } else if !self
            .backend
            .session_filesystems()?
            .contains_key(&actual_child_session_id)
        {
            let filesystem = self
                .backend
                .create_session_filesystem(actual_snapshot.workspace_id, actual_child_session_id)?;
            self.backend
                .session_filesystems()?
                .insert(actual_child_session_id, filesystem);
        }
        let child = ProjectAgentRecord {
            session_id: actual_child_session_id,
            project_id,
            parent_session_id: Some(parent_session_id),
            depth: parent.depth + 1,
            state: actual_snapshot.state,
            task_summary: Some(persisted_task.intent.clone()),
            output_cursor: EventSequence::default(),
            updated_at: actual_snapshot.updated_at,
        };
        if let Some(mut worktree) =
            persistence.load_project_worktree_by_task(persisted_task.task_id)?
        {
            self.ensure_project_worktree_ready(&mut worktree)?;
        }
        if was_created {
            let sequence = self.backend.journal()?.next();
            self.backend.journal()?.append_event(ServerEventEnvelope {
                protocol_version: CURRENT_PROTOCOL_VERSION,
                sequence,
                session_id: parent_session_id,
                event: ServerEvent::ProjectAgentCreated {
                    agent: child.clone(),
                },
            });
            let sequence = self.backend.journal()?.next();
            self.backend.journal()?.append_event(ServerEventEnvelope {
                protocol_version: CURRENT_PROTOCOL_VERSION,
                sequence,
                session_id: parent_session_id,
                event: ServerEvent::ProjectTaskUpdated {
                    task: persisted_task.clone(),
                },
            });
        }
        drop(state_persist_guard);
        drop(admission_guard);
        self.drain_workspace_project_admissions_locked(parent_snapshot.workspace_id, false)?;
        if let Some(updated_task) = persistence.load_delegated_task(persisted_task.task_id)? {
            persisted_task = updated_task;
        }
        drop(workspace_admission_guard);
        Ok(ServerResponse::ProjectChildCreated {
            task: persisted_task,
            child,
        })
    }

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
                            let ServerResponse::AgentRun(snapshot) = response else {
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
                        let ServerResponse::AgentRun(snapshot) = response else {
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
                let ServerResponse::AgentRun(snapshot) = response else {
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
                        let ServerResponse::AgentRun(snapshot) = response else {
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
                                let ServerResponse::AgentRun(stopped) = response else {
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
                    if self
                        .backend
                        .project_cancellation_failpoint
                        .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |remaining| {
                            if remaining > 0 {
                                Some(remaining - 1)
                            } else {
                                None
                            }
                        })
                        .is_ok_and(|remaining| remaining == 1)
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

    pub(crate) fn recover_pending_project_cancellation_cascades(&self) -> Result<()> {
        let Some(persistence) = self.backend.persistence.as_ref() else {
            return Ok(());
        };
        for cascade in persistence.list_pending_project_cancellation_cascades()? {
            self.apply_project_cancellation_cascade(&cascade)?;
        }
        Ok(())
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
                    let ServerResponse::AgentRun(stopped) = response else {
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
        persistence: &FilePersistence,
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

    pub(crate) fn load_project_child_worktree(
        &self,
        project_id: ProjectId,
        manager_session_id: AgentSessionId,
        task_id: loom_core::TaskId,
    ) -> Result<(loom_core::DelegatedTaskRecord, ProjectWorktreeRecord)> {
        let persistence = self.backend.persistence.as_ref().ok_or_else(|| {
            LoomError::new(
                ErrorCode::UnsupportedCapability,
                "project worktrees require durable storage",
                false,
            )
        })?;
        let project = self.load_project_snapshot(project_id)?;
        let task = persistence
            .load_delegated_task(task_id)?
            .ok_or_else(|| LoomError::not_found("delegated task", task_id))?;
        if task.project_id != project_id
            || task.requester_session_id != manager_session_id
            || !task.code_change
            || !project.agents.iter().any(|agent| {
                agent.session_id == task.target_session_id
                    && agent.parent_session_id == Some(manager_session_id)
            })
        {
            return Err(LoomError::not_found("project child task", task_id));
        }
        let worktree = persistence
            .load_project_worktree_by_task(task_id)?
            .ok_or_else(|| {
                LoomError::new(
                    ErrorCode::RecoveryRequired,
                    "code task is missing its durable worktree record",
                    true,
                )
            })?;
        if worktree.project_id != project_id
            || worktree.parent_session_id != manager_session_id
            || worktree.child_session_id != task.target_session_id
        {
            return Err(LoomError::new(
                ErrorCode::Persistence,
                "project worktree ownership does not match its task",
                false,
            ));
        }
        Ok((task, worktree))
    }

    pub(crate) fn open_project_child_worktree(
        &self,
        worktree: &ProjectWorktreeRecord,
    ) -> Result<GitService> {
        if worktree.status == ProjectWorktreeStatus::Removed {
            return Err(LoomError::invalid_state(
                "project child worktree has been removed",
            ));
        }
        let parent_repository = self
            .backend
            .session_repositories()?
            .get(&worktree.parent_session_id)
            .and_then(|repositories| repositories.get(&worktree.parent_repository_id))
            .cloned()
            .ok_or_else(|| {
                LoomError::new(
                    ErrorCode::RecoveryRequired,
                    "parent repository for project worktree is unavailable",
                    true,
                )
            })?;
        let parent_git =
            self.session_git(worktree.parent_session_id, worktree.parent_repository_id)?;
        let child_filesystem = self.session_filesystem(worktree.child_session_id)?;
        let relative_path = checked_session_relative_path(&worktree.relative_path)?;
        let destination = child_filesystem.root().join(relative_path);
        let metadata = fs::symlink_metadata(&destination).map_err(|error| {
            LoomError::new(
                ErrorCode::RecoveryRequired,
                format!("project child worktree path is unavailable: {error}"),
                true,
            )
        })?;
        if metadata.file_type().is_symlink() {
            return Err(LoomError::new(
                ErrorCode::WorkspaceAccessDenied,
                "refusing to open a project child worktree through a symlink",
                false,
            ));
        }
        let root = fs::canonicalize(child_filesystem.root()).map_err(|error| {
            LoomError::new(
                ErrorCode::WorkspaceAccessDenied,
                format!("could not resolve child filesystem root: {error}"),
                false,
            )
        })?;
        let canonical_destination = fs::canonicalize(&destination).map_err(|error| {
            LoomError::new(
                ErrorCode::RecoveryRequired,
                format!("could not resolve project child worktree path: {error}"),
                true,
            )
        })?;
        if !canonical_destination.starts_with(&root) {
            return Err(LoomError::new(
                ErrorCode::WorkspaceAccessDenied,
                "project worktree path escapes the child filesystem root",
                false,
            ));
        }
        let service = parent_git.open_linked_worktree(&worktree.worktree_name, &destination)?;
        let status = service.status()?;
        if status.branch.as_deref() != Some(worktree.branch_name.as_str()) || status.head.is_none()
        {
            return Err(LoomError::new(
                ErrorCode::RecoveryRequired,
                "linked project worktree does not match its durable branch identity",
                true,
            ));
        }
        let child_repository = SessionRepository {
            id: worktree.child_repository_id,
            source: parent_repository.source,
            path: worktree.relative_path.clone(),
            revision: status.head,
            attached_at: worktree.created_at,
        };
        self.backend
            .session_repositories()?
            .entry(worktree.child_session_id)
            .or_default()
            .insert(worktree.child_repository_id, child_repository);
        self.backend.session_vcs()?.insert(
            (worktree.child_session_id, worktree.child_repository_id),
            service.clone(),
        );
        child_filesystem.mark_state_dirty()?;
        Ok(service)
    }

    pub(crate) fn get_project_child_review(
        &self,
        project_id: ProjectId,
        manager_session_id: AgentSessionId,
        task_id: loom_core::TaskId,
    ) -> Result<ServerResponse> {
        let admission = self.backend.admissions.project(project_id)?;
        let _admission = admission.lock().map_err(|_| {
            LoomError::new(
                ErrorCode::Internal,
                "project worktree lock was poisoned",
                true,
            )
        })?;
        if !self.project_agent_permission_enabled_for_session(
            manager_session_id,
            Capability::ReadProjectChildReview,
            ProjectAgentPermission::Review,
        )? {
            return Err(LoomError::new(
                ErrorCode::AuthorizationDenied,
                "project child review grant is no longer valid",
                false,
            ));
        }
        let (_task, mut worktree) =
            self.load_project_child_worktree(project_id, manager_session_id, task_id)?;
        if matches!(
            worktree.status,
            ProjectWorktreeStatus::CleanupPending | ProjectWorktreeStatus::Removed
        ) {
            return Err(LoomError::invalid_state(
                "project child worktree is being removed or has been removed",
            ));
        }
        let service = self.open_project_child_worktree(&worktree)?;
        let status = service.status()?;
        let diff = service.diff_from_revision(&worktree.base_revision, MAX_REVIEW_DIFF_BYTES)?;
        if worktree.result_revision != status.head {
            worktree.result_revision = status.head.clone();
            worktree.updated_at = Timestamp::now();
            self.save_project_worktree_state(&worktree)?;
        }
        Ok(ServerResponse::ProjectChildReview {
            worktree,
            status,
            diff,
        })
    }

    pub(crate) fn integrate_project_child(
        &self,
        _request_id: RequestId,
        project_id: ProjectId,
        manager_session_id: AgentSessionId,
        task_id: loom_core::TaskId,
        expected_parent_revision: String,
    ) -> Result<ServerResponse> {
        if !self.project_agent_permission_enabled_for_session(
            manager_session_id,
            Capability::IntegrateProjectChild,
            ProjectAgentPermission::Integration,
        )? {
            return Err(LoomError::new(
                ErrorCode::AuthorizationDenied,
                "project child integration grant is no longer valid",
                false,
            ));
        }
        let admission = self.backend.admissions.project(project_id)?;
        let _admission = admission.lock().map_err(|_| {
            LoomError::new(
                ErrorCode::Internal,
                "project worktree lock was poisoned",
                true,
            )
        })?;
        let (task, mut worktree) =
            self.load_project_child_worktree(project_id, manager_session_id, task_id)?;
        if expected_parent_revision != worktree.base_revision {
            return Err(LoomError::conflict(format!(
                "child worktree was based on {}, not requested parent revision {}",
                worktree.base_revision, expected_parent_revision
            )));
        }
        if task.status != loom_core::DelegatedTaskStatus::Completed {
            return Err(LoomError::invalid_state(
                "a project child can be integrated only after its task completes",
            ));
        }
        if worktree.status == ProjectWorktreeStatus::Integrated {
            return Ok(ServerResponse::ProjectChildWorktreeUpdated(worktree));
        }
        if !matches!(
            worktree.status,
            ProjectWorktreeStatus::Ready
                | ProjectWorktreeStatus::Stale
                | ProjectWorktreeStatus::Integrating
        ) {
            return Err(LoomError::invalid_state(format!(
                "project child worktree in {:?} state cannot be integrated",
                worktree.status
            )));
        }

        let child_git = self.open_project_child_worktree(&worktree)?;
        let child_status = child_git.status()?;
        if child_status.branch.as_deref() != Some(worktree.branch_name.as_str()) {
            return Err(LoomError::new(
                ErrorCode::RecoveryRequired,
                "project child checkout is no longer on its assigned branch",
                true,
            ));
        }
        if !child_status.clean {
            worktree.status = if child_status.conflicts.is_empty() {
                ProjectWorktreeStatus::Ready
            } else {
                ProjectWorktreeStatus::Conflict
            };
            worktree.conflict_paths = child_status.conflicts.clone();
            worktree.error = Some(
                "project child checkout has uncommitted changes; commit or resolve them before integration"
                    .to_owned(),
            );
            worktree.updated_at = Timestamp::now();
            self.save_project_worktree_state(&worktree)?;
            return Err(LoomError::conflict(
                "project child checkout has uncommitted changes; commit or resolve them before integration",
            ));
        }
        let child_revision = child_status.head.ok_or_else(|| {
            LoomError::new(
                ErrorCode::RecoveryRequired,
                "project child checkout has no commit at HEAD",
                true,
            )
        })?;
        if child_revision == worktree.base_revision {
            return Err(LoomError::invalid_state(
                "project child has no committed changes to integrate",
            ));
        }
        if worktree.status == ProjectWorktreeStatus::Integrating
            && worktree.result_revision.as_deref() != Some(child_revision.as_str())
        {
            worktree.status = ProjectWorktreeStatus::RecoveryRequired;
            worktree.error = Some(
                "child branch changed while a prior integration was being recovered".to_owned(),
            );
            worktree.updated_at = Timestamp::now();
            self.save_project_worktree_state(&worktree)?;
            return Err(LoomError::new(
                ErrorCode::RecoveryRequired,
                worktree.error.clone().unwrap_or_default(),
                true,
            ));
        }
        if worktree.status != ProjectWorktreeStatus::Integrating
            && worktree.result_revision.as_deref() != Some(child_revision.as_str())
        {
            return Err(LoomError::invalid_state(
                "review the current committed child revision before integration",
            ));
        }

        let parent_git =
            self.session_git(worktree.parent_session_id, worktree.parent_repository_id)?;
        let parent_status = parent_git.status()?;
        if !parent_status.clean || parent_status.branch.is_none() {
            return Err(LoomError::conflict(
                "parent checkout must be clean and on a local branch before integration",
            ));
        }
        let parent_revision = parent_status.head.ok_or_else(|| {
            LoomError::invalid_state("parent repository HEAD does not point to a commit")
        })?;

        if worktree.status == ProjectWorktreeStatus::Integrating {
            if parent_revision == child_revision {
                worktree.status = ProjectWorktreeStatus::Integrated;
                worktree.integrated_revision = Some(child_revision);
                worktree.error = None;
                worktree.updated_at = Timestamp::now();
                self.save_project_worktree_state(&worktree)?;
                return Ok(ServerResponse::ProjectChildWorktreeUpdated(worktree));
            }
            if parent_revision != worktree.base_revision {
                worktree.status = ProjectWorktreeStatus::RecoveryRequired;
                worktree.error = Some(format!(
                    "parent checkout is at {parent_revision} while recovering integration of {}",
                    worktree
                        .result_revision
                        .as_deref()
                        .unwrap_or("unknown revision")
                ));
                worktree.updated_at = Timestamp::now();
                self.save_project_worktree_state(&worktree)?;
                return Err(LoomError::new(
                    ErrorCode::RecoveryRequired,
                    worktree.error.clone().unwrap_or_default(),
                    true,
                ));
            }
        }

        if parent_revision != worktree.base_revision {
            worktree.status = ProjectWorktreeStatus::Stale;
            worktree.error = Some(format!(
                "parent checkout advanced from {} to {parent_revision}; child changes were preserved",
                worktree.base_revision
            ));
            worktree.updated_at = Timestamp::now();
            self.save_project_worktree_state(&worktree)?;
            return Err(LoomError::conflict(
                "parent checkout advanced since this child worktree was created; child changes were preserved",
            ));
        }

        worktree.result_revision = Some(child_revision.clone());
        worktree.status = ProjectWorktreeStatus::Integrating;
        worktree.error = None;
        worktree.updated_at = Timestamp::now();
        self.save_project_worktree_state(&worktree)?;
        match parent_git.advance_clean_head_revisions(&worktree.base_revision, &child_revision) {
            Ok(integrated_revision) => {
                worktree.status = ProjectWorktreeStatus::Integrated;
                worktree.integrated_revision = Some(integrated_revision);
                worktree.error = None;
                worktree.updated_at = Timestamp::now();
                self.save_project_worktree_state(&worktree)?;
                Ok(ServerResponse::ProjectChildWorktreeUpdated(worktree))
            }
            Err(error) => {
                // Keep the integration intent retryable. A retry can detect a
                // completed fast-forward, safely retry from the base, or move
                // the record to recovery-required if the parent diverged.
                worktree.error = Some(error.message.clone());
                worktree.updated_at = Timestamp::now();
                self.save_project_worktree_state(&worktree)?;
                Err(error)
            }
        }
    }

    pub(crate) fn cleanup_project_child_worktree(
        &self,
        project_id: ProjectId,
        manager_session_id: AgentSessionId,
        task_id: loom_core::TaskId,
        disposition: ProjectWorktreeCleanupDisposition,
    ) -> Result<ServerResponse> {
        if !self
            .backend
            .supported_capabilities
            .contains(Capability::CleanupProjectChildWorktree)
        {
            return Err(LoomError::new(
                ErrorCode::UnsupportedCapability,
                "project child worktree cleanup is unavailable",
                false,
            ));
        }
        let admission = self.backend.admissions.project(project_id)?;
        let _admission = admission.lock().map_err(|_| {
            LoomError::new(
                ErrorCode::Internal,
                "project worktree lock was poisoned",
                true,
            )
        })?;
        let persistence = self.backend.persistence.as_ref().ok_or_else(|| {
            LoomError::new(
                ErrorCode::UnsupportedCapability,
                "project worktrees require durable storage",
                false,
            )
        })?;
        let (task, mut worktree) =
            self.load_project_child_worktree(project_id, manager_session_id, task_id)?;
        if !matches!(
            task.status,
            loom_core::DelegatedTaskStatus::Completed
                | loom_core::DelegatedTaskStatus::Failed
                | loom_core::DelegatedTaskStatus::Cancelled
        ) {
            return Err(LoomError::invalid_state(
                "a project child worktree can be cleaned up only after its task is terminal",
            ));
        }
        if worktree.status == ProjectWorktreeStatus::Removed {
            return Ok(ServerResponse::ProjectChildWorktreeUpdated(worktree));
        }
        if worktree.status == ProjectWorktreeStatus::CleanupPending
            && worktree.cleanup_disposition != Some(disposition)
            && worktree.error.is_none()
        {
            return Err(LoomError::conflict(
                "cleanup is still pending with a different disposition; retry that operation before changing its disposition",
            ));
        }

        worktree.cleanup_disposition = Some(disposition);
        worktree.error = None;
        worktree.updated_at = Timestamp::now();
        if disposition == ProjectWorktreeCleanupDisposition::Retain {
            self.open_project_child_worktree(&worktree)?;
            worktree.status = ProjectWorktreeStatus::Retained;
            self.save_project_worktree_state(&worktree)?;
            return Ok(ServerResponse::ProjectChildWorktreeUpdated(worktree));
        }

        worktree.status = ProjectWorktreeStatus::CleanupPending;
        self.save_project_worktree_state(&worktree)?;
        let parent_git =
            self.session_git(worktree.parent_session_id, worktree.parent_repository_id)?;
        let child_filesystem = self.session_filesystem(worktree.child_session_id)?;
        let destination = child_filesystem
            .root()
            .join(checked_session_relative_path(&worktree.relative_path)?);
        let force = disposition == ProjectWorktreeCleanupDisposition::DiscardChanges;
        let removal = parent_git.remove_linked_worktree(&worktree.worktree_name, force);
        if let Err(error) = removal {
            let already_removed = error.code == ErrorCode::NotFound
                && !destination.exists()
                && worktree.status == ProjectWorktreeStatus::CleanupPending;
            if !already_removed {
                worktree.error = Some(error.message.clone());
                worktree.updated_at = Timestamp::now();
                self.save_project_worktree_state(&worktree)?;
                return Err(error);
            }
        }
        self.backend
            .session_repositories()?
            .entry(worktree.child_session_id)
            .or_default()
            .remove(&worktree.child_repository_id);
        self.backend
            .session_vcs()?
            .remove(&(worktree.child_session_id, worktree.child_repository_id));
        child_filesystem.mark_state_dirty()?;
        worktree.status = ProjectWorktreeStatus::Removed;
        worktree.error = None;
        worktree.updated_at = Timestamp::now();
        persistence.save_project_worktree(&worktree)?;
        let sequence = self.backend.journal()?.next();
        self.backend.journal()?.append_event(ServerEventEnvelope {
            protocol_version: CURRENT_PROTOCOL_VERSION,
            sequence,
            session_id: worktree.parent_session_id,
            event: ServerEvent::ProjectChildWorktreeUpdated {
                worktree: worktree.clone(),
            },
        });
        Ok(ServerResponse::ProjectChildWorktreeUpdated(worktree))
    }

    pub(crate) fn save_project_worktree_state(
        &self,
        worktree: &ProjectWorktreeRecord,
    ) -> Result<()> {
        let persistence = self.backend.persistence.as_ref().ok_or_else(|| {
            LoomError::new(
                ErrorCode::UnsupportedCapability,
                "project worktrees require durable storage",
                false,
            )
        })?;
        persistence.save_project_worktree(worktree)?;
        let sequence = self.backend.journal()?.next();
        self.backend.journal()?.append_event(ServerEventEnvelope {
            protocol_version: CURRENT_PROTOCOL_VERSION,
            sequence,
            session_id: worktree.parent_session_id,
            event: ServerEvent::ProjectChildWorktreeUpdated {
                worktree: worktree.clone(),
            },
        });
        Ok(())
    }

    pub(crate) fn ensure_project_worktree_ready(
        &self,
        worktree: &mut ProjectWorktreeRecord,
    ) -> Result<()> {
        if !matches!(
            worktree.status,
            ProjectWorktreeStatus::Creating | ProjectWorktreeStatus::Ready
        ) {
            return Err(LoomError::new(
                ErrorCode::RecoveryRequired,
                format!(
                    "project child worktree is {:?} and cannot start a run",
                    worktree.status
                ),
                true,
            ));
        }
        if self.backend.persistence.is_none() {
            return Err(LoomError::new(
                ErrorCode::UnsupportedCapability,
                "project worktrees require durable storage",
                false,
            ));
        }
        let parent_repository = self
            .backend
            .session_repositories()?
            .get(&worktree.parent_session_id)
            .and_then(|repositories| repositories.get(&worktree.parent_repository_id))
            .cloned()
            .ok_or_else(|| {
                LoomError::new(
                    ErrorCode::RecoveryRequired,
                    "parent repository for project worktree is unavailable",
                    true,
                )
            })?;
        let parent_git =
            self.session_git(worktree.parent_session_id, worktree.parent_repository_id)?;
        let child_filesystem = if let Some(filesystem) = self
            .backend
            .session_filesystems()?
            .get(&worktree.child_session_id)
            .cloned()
        {
            filesystem
        } else if self
            .backend
            .persisted_session_filesystems()?
            .contains(&worktree.child_session_id)
        {
            self.session_filesystem(worktree.child_session_id)?
        } else {
            let child = self.backend.sessions()?.get(worktree.child_session_id)?;
            let filesystem = self
                .backend
                .create_session_filesystem(child.workspace_id, worktree.child_session_id)?;
            self.backend
                .session_filesystems()?
                .insert(worktree.child_session_id, filesystem.clone());
            filesystem
        };
        let relative_path = checked_session_relative_path(&worktree.relative_path)?;
        let destination = child_filesystem.root().join(&relative_path);
        let destination_parent = destination.parent().ok_or_else(|| {
            LoomError::invalid_request("project worktree path must have a parent directory")
        })?;
        fs::create_dir_all(destination_parent).map_err(|error| {
            LoomError::new(
                ErrorCode::WorkspaceAccessDenied,
                format!("could not create project worktree parent directory: {error}"),
                false,
            )
        })?;
        let root = fs::canonicalize(child_filesystem.root()).map_err(|error| {
            LoomError::new(
                ErrorCode::WorkspaceAccessDenied,
                format!("could not resolve child filesystem root: {error}"),
                false,
            )
        })?;
        let canonical_parent = fs::canonicalize(destination_parent).map_err(|error| {
            LoomError::new(
                ErrorCode::WorkspaceAccessDenied,
                format!("could not resolve project worktree parent directory: {error}"),
                false,
            )
        })?;
        if !canonical_parent.starts_with(&root) {
            return Err(LoomError::new(
                ErrorCode::WorkspaceAccessDenied,
                "project worktree path escapes the child filesystem root",
                false,
            ));
        }

        let worktree_service = if destination.exists() {
            parent_git.open_linked_worktree(&worktree.worktree_name, &destination)
        } else if worktree.status == ProjectWorktreeStatus::Creating {
            parent_git.create_linked_worktree_at_revision(
                &worktree.worktree_name,
                &worktree.branch_name,
                &destination,
                &worktree.base_revision,
            )
        } else {
            Err(LoomError::new(
                ErrorCode::RecoveryRequired,
                "a ready project worktree is missing; refusing to recreate it as an empty checkout",
                true,
            ))
        };
        let worktree_service = match worktree_service {
            Ok(worktree_service) => worktree_service,
            Err(error) => {
                worktree.status = ProjectWorktreeStatus::RecoveryRequired;
                worktree.error = Some(error.message.clone());
                worktree.updated_at = Timestamp::now();
                self.save_project_worktree_state(worktree)?;
                return Err(error);
            }
        };
        let checkout_status = worktree_service.status()?;
        if checkout_status.branch.as_deref() != Some(worktree.branch_name.as_str())
            || checkout_status.head.as_deref().is_none_or(|head| {
                worktree.status == ProjectWorktreeStatus::Creating && head != worktree.base_revision
            })
        {
            let error = LoomError::new(
                ErrorCode::RecoveryRequired,
                "linked project worktree does not match its durable branch and base intent",
                true,
            );
            worktree.status = ProjectWorktreeStatus::RecoveryRequired;
            worktree.error = Some(error.message.clone());
            worktree.updated_at = Timestamp::now();
            self.save_project_worktree_state(worktree)?;
            return Err(error);
        }

        let child_repository = SessionRepository {
            id: worktree.child_repository_id,
            source: parent_repository.source,
            path: worktree.relative_path.clone(),
            revision: checkout_status.head,
            attached_at: worktree.created_at,
        };
        self.backend
            .session_repositories()?
            .entry(worktree.child_session_id)
            .or_default()
            .insert(worktree.child_repository_id, child_repository);
        self.backend.session_vcs()?.insert(
            (worktree.child_session_id, worktree.child_repository_id),
            worktree_service,
        );
        child_filesystem.mark_state_dirty()?;
        if worktree.status == ProjectWorktreeStatus::Creating {
            worktree.status = ProjectWorktreeStatus::Ready;
            worktree.error = None;
            worktree.updated_at = Timestamp::now();
            self.save_project_worktree_state(worktree)?;
        }
        Ok(())
    }

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
            let sequence = self.backend.journal()?.next();
            self.backend.journal()?.append_event(ServerEventEnvelope {
                protocol_version: CURRENT_PROTOCOL_VERSION,
                sequence,
                session_id: updated_task.requester_session_id,
                event: ServerEvent::ProjectTaskUpdated { task: updated_task },
            });
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
                let sequence = self.backend.journal()?.next();
                self.backend.journal()?.append_event(ServerEventEnvelope {
                    protocol_version: CURRENT_PROTOCOL_VERSION,
                    sequence,
                    session_id: updated_task.requester_session_id,
                    event: ServerEvent::ProjectTaskUpdated { task: updated_task },
                });
            }
            return Err(error);
        }
        Ok(())
    }
}
