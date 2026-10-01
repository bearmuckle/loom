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
        let worktrees_supported = self
            .backend
            .supported_capabilities
            .contains(Capability::CreateProjectWorktree);
        if spec.code_change && !worktrees_supported {
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
            // A check-out is provisioned for every task when the parent has one
            // clean repository, so recover it whenever it exists rather than
            // only for code tasks.
            if let Some(mut worktree) =
                persistence.load_project_worktree_by_task(existing_task.task_id)?
            {
                self.ensure_project_worktree_ready(&mut worktree)?;
            } else if existing_task.code_change {
                return Err(LoomError::new(
                    ErrorCode::RecoveryRequired,
                    "code task is missing its durable worktree intent",
                    true,
                ));
            }
            self.schedule_project_task_if_ready(&mut existing_task)?;
            return Ok(ServerResponse::Project(
                ProjectResponse::ProjectChildCreated {
                    task: existing_task,
                    child,
                },
            ));
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
        // A check-out is provisioned for every delegated task, not only code
        // tasks, whenever the parent exposes exactly one clean repository.
        // `code_change` selects write authority; the presence of a check-out no
        // longer implies the child may commit or be reviewed and integrated.
        // Code tasks still require a check-out, while read-only tasks degrade to
        // a prompt-only child when no suitable repository is available.
        if worktrees_supported {
            let repositories = self
                .backend
                .session_repositories()?
                .get(&parent_session_id)
                .cloned()
                .unwrap_or_default();
            let exactly_one_repository = repositories.len() == 1;
            let mut checkout = None;
            if let Some((parent_repository_id, _)) = repositories.into_iter().next() {
                let parent_git = self.session_git(parent_session_id, parent_repository_id)?;
                let parent_status = parent_git.status()?;
                if parent_status.clean
                    && parent_status.branch.is_some()
                    && let Some(base_revision) = parent_status.head.clone()
                {
                    checkout = Some((parent_repository_id, base_revision));
                }
            }
            match checkout {
                Some((parent_repository_id, base_revision)) => {
                    let child_repository_id = RepositoryId::new();
                    let child_filesystem = self.backend.create_session_filesystem(
                        parent_snapshot.workspace_id,
                        child_session_id,
                    )?;
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
                None if task.code_change => {
                    return Err(if exactly_one_repository {
                        LoomError::conflict(
                            "code tasks require a clean parent checkout on a local branch",
                        )
                    } else {
                        LoomError::invalid_request(
                            "code tasks currently require exactly one Git repository attached to the parent session",
                        )
                    });
                }
                None => {}
            }
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
        Ok(ServerResponse::Project(
            ProjectResponse::ProjectChildCreated {
                task: persisted_task,
                child,
            },
        ))
    }
}
