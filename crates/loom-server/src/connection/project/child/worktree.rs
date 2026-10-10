use super::*;

impl InProcessConnection {
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
        let (task, mut worktree) =
            self.load_project_child_worktree(project_id, manager_session_id, task_id)?;
        if !task.code_change {
            return Err(LoomError::not_found("project child code task", task_id));
        }
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
        Ok(ServerResponse::Project(
            ProjectResponse::ProjectChildReview {
                worktree,
                status,
                diff,
            },
        ))
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
        if !task.code_change {
            return Err(LoomError::not_found("project child code task", task_id));
        }
        if task.status != loom_core::DelegatedTaskStatus::Completed {
            return Err(LoomError::invalid_state(
                "a project child can be integrated only after its task completes",
            ));
        }
        if worktree.status == ProjectWorktreeStatus::Integrated {
            return Ok(ServerResponse::Project(
                ProjectResponse::ProjectChildWorktreeUpdated(worktree),
            ));
        }
        if !matches!(
            worktree.status,
            ProjectWorktreeStatus::Ready
                | ProjectWorktreeStatus::Stale
                | ProjectWorktreeStatus::Conflict
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
        if parent_git.operation_in_progress()? {
            worktree.status = ProjectWorktreeStatus::RecoveryRequired;
            worktree.error = Some(
                "parent checkout has an in-progress Git operation; resolve it before integrating"
                    .to_owned(),
            );
            worktree.updated_at = Timestamp::now();
            self.save_project_worktree_state(&worktree)?;
            return Err(LoomError::new(
                ErrorCode::RecoveryRequired,
                worktree.error.clone().unwrap_or_default(),
                true,
            ));
        }
        let parent_status = parent_git.status()?;
        if !parent_status.clean || parent_status.branch.is_none() {
            return Err(LoomError::conflict(
                "parent checkout must be clean and on a local branch before integration",
            ));
        }
        let parent_revision = parent_status.head.ok_or_else(|| {
            LoomError::invalid_state("parent repository HEAD does not point to a commit")
        })?;

        // The caller reviewed against `expected_parent_revision`; it must be in
        // the parent's history so we never integrate onto a rewritten or
        // unrelated checkout. The parent is otherwise free to have advanced.
        if !parent_git.is_ancestor_revision(&expected_parent_revision, &parent_revision)? {
            return Err(LoomError::conflict(format!(
                "parent checkout at {parent_revision} does not descend from the expected revision {expected_parent_revision}; re-review before integrating"
            )));
        }

        // A prior attempt may have integrated the child without recording it,
        // for example if it was interrupted mid-integration. Completing that
        // record is idempotent and never replays the merge.
        if parent_git.is_ancestor_revision(&child_revision, &parent_revision)? {
            worktree.status = ProjectWorktreeStatus::Integrated;
            worktree.result_revision = Some(child_revision);
            worktree.integrated_revision = Some(parent_revision);
            worktree.conflict_paths.clear();
            worktree.error = None;
            worktree.updated_at = Timestamp::now();
            self.save_project_worktree_state(&worktree)?;
            return Ok(ServerResponse::Project(
                ProjectResponse::ProjectChildWorktreeUpdated(worktree),
            ));
        }

        worktree.result_revision = Some(child_revision.clone());
        worktree.conflict_paths.clear();
        worktree.status = ProjectWorktreeStatus::Integrating;
        worktree.error = None;
        worktree.updated_at = Timestamp::now();
        self.save_project_worktree_state(&worktree)?;

        let message = format!(
            "loom: integrate project child task {} ({})",
            task.task_id, task.child_name
        );
        match parent_git.integrate_merge_revisions(&parent_revision, &child_revision, &message) {
            Ok(loom_vcs::MergeIntegrationOutcome::AlreadyPresent) => {
                self.finish_project_child_integration(&mut worktree, &parent_revision)
            }
            Ok(loom_vcs::MergeIntegrationOutcome::FastForward(revision))
            | Ok(loom_vcs::MergeIntegrationOutcome::Merged(revision)) => {
                let revision = revision.to_string();
                self.finish_project_child_integration(&mut worktree, &revision)
            }
            Ok(loom_vcs::MergeIntegrationOutcome::Conflicted(paths)) => {
                worktree.status = ProjectWorktreeStatus::Conflict;
                worktree.conflict_paths = paths.clone();
                worktree.error = Some(format!(
                    "integration conflicted in {}; the parent checkout and child worktree were preserved",
                    paths.join(", ")
                ));
                worktree.updated_at = Timestamp::now();
                self.save_project_worktree_state(&worktree)?;
                Err(LoomError::conflict(
                    worktree.error.clone().unwrap_or_default(),
                ))
            }
            Err(error) => {
                // Keep the integration intent retryable. A retry detects an
                // already-integrated child, safely retries the merge, or moves
                // the record to recovery-required when the parent checkout
                // needs explicit attention.
                worktree.error = Some(error.message.clone());
                if error.code == ErrorCode::RecoveryRequired {
                    worktree.status = ProjectWorktreeStatus::RecoveryRequired;
                }
                worktree.updated_at = Timestamp::now();
                self.save_project_worktree_state(&worktree)?;
                Err(error)
            }
        }
    }

    fn finish_project_child_integration(
        &self,
        worktree: &mut ProjectWorktreeRecord,
        integrated_revision: &str,
    ) -> Result<ServerResponse> {
        worktree.status = ProjectWorktreeStatus::Integrated;
        worktree.integrated_revision = Some(integrated_revision.to_owned());
        worktree.conflict_paths.clear();
        worktree.error = None;
        worktree.updated_at = Timestamp::now();
        self.save_project_worktree_state(worktree)?;
        Ok(ServerResponse::Project(
            ProjectResponse::ProjectChildWorktreeUpdated(worktree.clone()),
        ))
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
            return Ok(ServerResponse::Project(
                ProjectResponse::ProjectChildWorktreeUpdated(worktree),
            ));
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
            return Ok(ServerResponse::Project(
                ProjectResponse::ProjectChildWorktreeUpdated(worktree),
            ));
        }

        worktree.status = ProjectWorktreeStatus::CleanupPending;
        self.save_project_worktree_state(&worktree)?;
        let force = disposition == ProjectWorktreeCleanupDisposition::DiscardChanges;
        if let Err(error) = self.remove_project_worktree_checkout(&worktree, force) {
            worktree.error = Some(error.message.clone());
            worktree.updated_at = Timestamp::now();
            self.save_project_worktree_state(&worktree)?;
            return Err(error);
        }
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
        Ok(ServerResponse::Project(
            ProjectResponse::ProjectChildWorktreeUpdated(worktree),
        ))
    }

    /// Removes one project worktree checkout and its in-memory registrations.
    ///
    /// The checkout is resolved through the child session filesystem when it is
    /// loaded and through the session root layout otherwise, because the child
    /// session row still exists at every call site. A worktree that is locked is
    /// refused unless `force` is true, and a registration that is already gone
    /// with its checkout is treated as removed. Any other failure is returned to
    /// the caller so it can abort without touching the session rows.
    pub(crate) fn remove_project_worktree_checkout(
        &self,
        worktree: &ProjectWorktreeRecord,
        force: bool,
    ) -> Result<()> {
        let parent_git =
            self.session_git(worktree.parent_session_id, worktree.parent_repository_id)?;
        let relative_path = checked_session_relative_path(&worktree.relative_path)?;
        let child_filesystem = self
            .backend
            .session_filesystems()?
            .get(&worktree.child_session_id)
            .cloned();
        let destination = match &child_filesystem {
            Some(filesystem) => filesystem.root().join(&relative_path),
            None => {
                let child = self.backend.sessions()?.get(worktree.child_session_id)?;
                self.backend
                    .session_root_base
                    .join(child.workspace_id.to_string())
                    .join(worktree.child_session_id.to_string())
                    .join("fs")
                    .join(&relative_path)
            }
        };
        if !force && parent_git.linked_worktree_is_locked(&worktree.worktree_name)? {
            return Err(LoomError::invalid_state(format!(
                "project worktree '{}' is locked; pass force only when its lock may be removed",
                worktree.worktree_name
            )));
        }
        if let Err(error) = parent_git.remove_linked_worktree(&worktree.worktree_name, force) {
            let already_removed = error.code == ErrorCode::NotFound && !destination.exists();
            if !already_removed {
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
        if let Some(filesystem) = child_filesystem {
            filesystem.mark_state_dirty()?;
        }
        Ok(())
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

    /// Reconciles worktrees left mid-integration by a crash or restart.
    ///
    /// An integration is finalized only when the parent checkout already
    /// contains the reviewed child revision. An in-progress Git operation on
    /// the parent is surfaced as recovery-required and never replayed or
    /// auto-resolved. Anything else is left for an explicit integration retry.
    pub(crate) fn reconcile_project_child_integrations(&self, project_id: ProjectId) -> Result<()> {
        let Some(persistence) = self.backend.persistence.as_ref() else {
            return Ok(());
        };
        for task in persistence.list_project_tasks(project_id)? {
            if !task.code_change {
                continue;
            }
            let Some(mut worktree) = persistence.load_project_worktree_by_task(task.task_id)?
            else {
                continue;
            };
            if worktree.status != ProjectWorktreeStatus::Integrating {
                continue;
            }
            let Some(child_revision) = worktree.result_revision.clone() else {
                continue;
            };
            let parent_git =
                self.session_git(worktree.parent_session_id, worktree.parent_repository_id)?;
            if parent_git.operation_in_progress()? {
                worktree.status = ProjectWorktreeStatus::RecoveryRequired;
                worktree.error = Some(
                    "parent checkout has an in-progress Git operation; resolve it before integrating"
                        .to_owned(),
                );
                worktree.updated_at = Timestamp::now();
                self.save_project_worktree_state(&worktree)?;
                continue;
            }
            let parent_status = parent_git.status()?;
            let Some(parent_revision) = parent_status.head else {
                continue;
            };
            if parent_git.is_ancestor_revision(&child_revision, &parent_revision)? {
                worktree.status = ProjectWorktreeStatus::Integrated;
                worktree.integrated_revision = Some(parent_revision);
                worktree.conflict_paths.clear();
                worktree.error = None;
                worktree.updated_at = Timestamp::now();
                self.save_project_worktree_state(&worktree)?;
            }
        }
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
}
