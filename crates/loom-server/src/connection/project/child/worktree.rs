use super::rebase::{
    paths_outside_set, project_child_rebase_paths, project_child_rebase_stat_paths,
};
use super::*;

/// Cuts one line of a child file to [`MAX_CHILD_FILE_LINE_CHARS`] and marks the
/// cut, so one minified line cannot dominate a bounded read.
fn bound_child_file_line(line: &str) -> String {
    match line.char_indices().nth(MAX_CHILD_FILE_LINE_CHARS) {
        Some((end, _)) => format!("{}… [loom: line truncated]", &line[..end]),
        None => line.to_owned(),
    }
}

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

    /// Runs `review` against a code child's opened checkout under the project
    /// lock, after checking the review grant and keeping the recorded result
    /// revision current.
    ///
    /// Every review path shares this setup, so requesting a summary, a bounded
    /// patch, or one file's line range can never see a checkout the grant does
    /// not cover or a record another operation is changing.
    fn with_reviewed_child_checkout<T>(
        &self,
        project_id: ProjectId,
        manager_session_id: AgentSessionId,
        task_id: loom_core::TaskId,
        review: impl FnOnce(
            &ProjectWorktreeRecord,
            &GitService,
            &loom_core::GitRepositoryStatus,
        ) -> Result<T>,
    ) -> Result<T> {
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
        if worktree.result_revision != status.head {
            worktree.result_revision = status.head.clone();
            worktree.updated_at = Timestamp::now();
            self.save_project_worktree_state(&worktree)?;
        }
        review(&worktree, &service, &status)
    }

    pub(crate) fn get_project_child_review(
        &self,
        project_id: ProjectId,
        manager_session_id: AgentSessionId,
        task_id: loom_core::TaskId,
    ) -> Result<ServerResponse> {
        let (worktree, status, diff) = self.with_reviewed_child_checkout(
            project_id,
            manager_session_id,
            task_id,
            |worktree, service, status| {
                Ok((
                    worktree.clone(),
                    status.clone(),
                    service.diff_from_revision(&worktree.base_revision, MAX_REVIEW_DIFF_BYTES)?,
                ))
            },
        )?;
        Ok(ServerResponse::Project(
            ProjectResponse::ProjectChildReview {
                worktree: Box::new(worktree),
                status,
                diff,
            },
        ))
    }

    /// Reviews a code child as a summary or as a bounded patch.
    ///
    /// The summary names every path with its kind of change and line counts,
    /// together with the commits the child made and the identity of each. The
    /// patch honours a path filter, a context size, and a per-file byte budget,
    /// and `MAX_REVIEW_DIFF_BYTES` bounds the whole patch whatever the caller
    /// asks for, so neither mode can return a change of any size.
    pub(crate) fn get_project_child_review_view(
        &self,
        project_id: ProjectId,
        manager_session_id: AgentSessionId,
        task_id: loom_core::TaskId,
        request: &ProjectChildReviewRequest<'_>,
    ) -> Result<ProjectChildReviewView> {
        self.with_reviewed_child_checkout(
            project_id,
            manager_session_id,
            task_id,
            |worktree, service, status| {
                let mut view = ProjectChildReviewView {
                    worktree: worktree.clone(),
                    status: status.clone(),
                    stat: Vec::new(),
                    commits: Vec::new(),
                    diff: None,
                };
                // The file list is what the declared scope is checked against,
                // so both modes build it: a summary lists it, and a patch
                // review still reports whether the change stays in scope.
                view.stat = service.review_stat_from_revision(&worktree.base_revision)?;
                match request.mode {
                    ProjectChildReviewMode::Stat => {
                        // A child that has not committed yet has no commits to
                        // name, and its uncommitted work is the summary.
                        if let Some(head) = status.head.as_deref()
                            && head != worktree.base_revision
                        {
                            view.commits =
                                service.patch_commits_since(&worktree.base_revision, head)?;
                        }
                    }
                    ProjectChildReviewMode::Diff => {
                        view.diff = Some(service.review_diff_from_revision(
                            &worktree.base_revision,
                            &loom_vcs::GitReviewDiffRequest {
                                paths: request.paths,
                                context: request.context,
                                max_bytes_per_file: request.max_bytes,
                                max_total_bytes: MAX_REVIEW_DIFF_BYTES,
                            },
                        )?);
                    }
                }
                Ok(view)
            },
        )
    }

    /// Reads an exact line range of one file in a code child's checkout.
    ///
    /// The path must stay inside the checkout: an absolute path, a parent
    /// traversal, and a symlink that resolves outside the checkout are all
    /// refused. The range must be inside the file, a range longer than
    /// [`MAX_CHILD_FILE_LINES`] is refused rather than silently shortened, and
    /// the returned text is bounded by [`MAX_CHILD_FILE_BYTES`].
    pub(crate) fn read_project_child_file(
        &self,
        project_id: ProjectId,
        manager_session_id: AgentSessionId,
        task_id: loom_core::TaskId,
        path: &str,
        line_start: Option<usize>,
        line_end: Option<usize>,
    ) -> Result<ProjectChildFileSlice> {
        self.with_reviewed_child_checkout(
            project_id,
            manager_session_id,
            task_id,
            |_worktree, service, _status| {
                let file = checked_session_path(service.root(), path)?;
                let metadata = fs::metadata(&file).map_err(|error| {
                    LoomError::invalid_request(format!(
                        "could not read '{path}' in the child checkout: {error}"
                    ))
                })?;
                if !metadata.is_file() {
                    return Err(LoomError::invalid_request(format!(
                        "'{path}' is not a file in the child checkout"
                    )));
                }
                if metadata.len() > MAX_CHILD_FILE_SIZE_BYTES {
                    return Err(LoomError::invalid_request(format!(
                        "'{path}' is {} bytes; read_child_file reads files up to {MAX_CHILD_FILE_SIZE_BYTES} bytes",
                        metadata.len()
                    )));
                }
                let content = fs::read(&file).map_err(|error| {
                    LoomError::invalid_request(format!(
                        "could not read '{path}' in the child checkout: {error}"
                    ))
                })?;
                let content = String::from_utf8(content).map_err(|_| {
                    LoomError::invalid_request(format!(
                        "'{path}' is not a UTF-8 text file, so read_child_file cannot show it"
                    ))
                })?;
                let all = content.lines().collect::<Vec<_>>();
                let total_lines = all.len();
                if total_lines == 0 {
                    return Err(LoomError::invalid_request(format!("'{path}' is empty")));
                }
                let start = line_start.unwrap_or(1);
                if start == 0 {
                    return Err(LoomError::invalid_request(
                        "line_start must be at least 1",
                    ));
                }
                if start > total_lines {
                    return Err(LoomError::invalid_request(format!(
                        "line_start {start} is past the {total_lines} lines of '{path}'"
                    )));
                }
                let end = match line_end {
                    Some(end) if end < start => {
                        return Err(LoomError::invalid_request(format!(
                            "line_end {end} is before line_start {start}"
                        )));
                    }
                    Some(end) => end,
                    None => start.saturating_add(MAX_CHILD_FILE_LINES - 1),
                };
                if end - start + 1 > MAX_CHILD_FILE_LINES {
                    return Err(LoomError::invalid_request(format!(
                        "a range of {} lines exceeds the {MAX_CHILD_FILE_LINES} line cap; request a smaller range",
                        end - start + 1
                    )));
                }
                let end = end.min(total_lines);
                let mut lines = Vec::new();
                let mut used = 0usize;
                let mut truncated = false;
                for line in &all[start - 1..end] {
                    let text = bound_child_file_line(line);
                    if used + text.len() + 1 > MAX_CHILD_FILE_BYTES {
                        truncated = true;
                        break;
                    }
                    used += text.len() + 1;
                    lines.push(text);
                }
                Ok(ProjectChildFileSlice {
                    path: path.to_owned(),
                    line_start: start,
                    // The last line actually returned, which the byte budget
                    // can cut short of the requested range.
                    line_end: start + lines.len().saturating_sub(1),
                    total_lines,
                    lines,
                    truncated,
                })
            },
        )
    }

    pub(crate) fn integrate_project_child(
        &self,
        _request_id: RequestId,
        project_id: ProjectId,
        manager_session_id: AgentSessionId,
        task_id: loom_core::TaskId,
        request: ProjectChildIntegrationRequest,
    ) -> Result<ServerResponse> {
        let ProjectChildIntegrationRequest {
            expected_parent_revision,
            scope_override,
            strategy,
            allow_merge,
        } = request;
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
                ProjectResponse::ProjectChildWorktreeUpdated(Box::new(worktree)),
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
        // The reviewed commit has to stay inside the file scope its delegation
        // declared. A commit that does not is refused with its offending paths
        // named, unless the manager explicitly overrides the declaration and
        // states why, which is recorded on the worktree.
        if let Some(scope) = worktree.scope.clone() {
            let stat = child_git.review_stat_from_revision(&worktree.base_revision)?;
            let mut out_of_scope = scope
                .out_of_scope(&stat)
                .map(|stat| stat.path.clone())
                .collect::<Vec<_>>();
            out_of_scope.sort();
            out_of_scope.dedup();
            if !out_of_scope.is_empty() {
                // The refusal names the offending paths, bounded the same way
                // a blocked parent checkout names its paths, so a wide change
                // cannot turn one error into pages of text.
                const MAX_NAMED_PATHS: usize = 20;
                let named = out_of_scope
                    .iter()
                    .take(MAX_NAMED_PATHS)
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(", ");
                let remaining = out_of_scope.len().saturating_sub(MAX_NAMED_PATHS);
                let named = if remaining > 0 {
                    format!("{named} and {remaining} more")
                } else {
                    named
                };
                match scope_override.as_ref() {
                    Some(scope_override) => {
                        worktree.scope_override = Some(loom_core::ProjectWorktreeScopeOverride {
                            reason: scope_override.reason.clone(),
                            paths: out_of_scope.clone(),
                            recorded_at: Timestamp::now(),
                        });
                        worktree.updated_at = Timestamp::now();
                        self.save_project_worktree_state(&worktree)?;
                    }
                    None => {
                        return Err(LoomError::invalid_state(format!(
                            "the reviewed commit changes paths the child's delegation left out of scope: {named}; review them, then integrate with allow_out_of_scope and a reason to land it anyway"
                        )));
                    }
                }
            }
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
            let mut paths = parent_status
                .files
                .iter()
                .map(|file| file.path.clone())
                .collect::<Vec<_>>();
            paths.sort();
            paths.dedup();
            return Err(LoomError::conflict(if paths.is_empty() {
                "parent checkout must be clean and on a local branch before integration".to_owned()
            } else {
                format!(
                    "parent checkout must be clean and on a local branch before integration; blocking paths: {}",
                    paths.join(", ")
                )
            }));
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
            worktree.result_revision = Some(child_revision.clone());
            worktree.integrated_revision = Some(parent_revision.clone());
            worktree.landed_by = Some(ProjectWorktreeLandedBy::Integrated);
            worktree.integration = Some(project_child_integration_record(
                strategy,
                &child_revision,
                &parent_revision,
                &parent_revision,
                0,
                false,
                Vec::new(),
            ));
            worktree.conflict_paths.clear();
            worktree.error = None;
            worktree.updated_at = Timestamp::now();
            self.save_project_worktree_state(&worktree)?;
            self.auto_archive_integrated_child(worktree.child_session_id);
            return Ok(ServerResponse::Project(
                ProjectResponse::ProjectChildWorktreeUpdated(Box::new(worktree)),
            ));
        }

        // The strategy decides what an integration may do, so a request it
        // cannot satisfy is refused before either checkout moves instead of
        // escalating to a strategy the caller did not ask for.
        match strategy {
            ProjectWorktreeIntegrationStrategy::FastForward => {
                if !parent_git.is_ancestor_revision(&parent_revision, &child_revision)? {
                    return Err(LoomError::invalid_state(format!(
                        "a fast-forward integration is not applicable: parent checkout {parent_revision} is not an ancestor of the reviewed child revision {child_revision}; integrate with strategy `rebase` to replay the child's commits onto the parent tip, or land the change yourself and record it with mark_landed"
                    )));
                }
            }
            ProjectWorktreeIntegrationStrategy::Rebase => {}
            ProjectWorktreeIntegrationStrategy::Merge => {
                if let Some(rule) = merge_commits_forbidden(parent_git.root())
                    && !allow_merge
                {
                    return Err(LoomError::invalid_state(format!(
                        "the parent repository's own instructions forbid merge commits, so a merge integration is refused: {rule}; integrate with strategy `rebase` to keep history linear, or pass allow_merge to override that rule"
                    )));
                }
            }
        }

        worktree.result_revision = Some(child_revision.clone());
        worktree.conflict_paths.clear();
        worktree.status = ProjectWorktreeStatus::Integrating;
        worktree.error = None;
        worktree.updated_at = Timestamp::now();
        self.save_project_worktree_state(&worktree)?;

        let message = project_child_integration_message(&child_git, &child_revision, &task);

        // A fast-forward advances the parent straight to the child's own
        // commits. A merge lets Git decide, and only a rebase replays the
        // child's commits onto the parent tip first, keeping each commit's
        // author and message and creating no merge commit.
        let mut replay = None;
        let outcome = match strategy {
            ProjectWorktreeIntegrationStrategy::FastForward
            | ProjectWorktreeIntegrationStrategy::Merge => {
                parent_git.integrate_merge_revisions(&parent_revision, &child_revision, &message)
            }
            ProjectWorktreeIntegrationStrategy::Rebase => {
                match self.replay_project_child_for_integration(
                    &mut worktree,
                    &child_git,
                    &parent_revision,
                    &child_revision,
                ) {
                    Ok(replayed) => {
                        let outcome = parent_git.integrate_merge_revisions(
                            &parent_revision,
                            &replayed.revision,
                            &message,
                        );
                        replay = Some(replayed);
                        outcome
                    }
                    Err(error) => {
                        // The replay refused before the parent moved, so the
                        // record names what stopped it and stays retryable.
                        worktree.status = project_child_integration_refusal_status(error.code);
                        worktree.error = Some(error.message.clone());
                        worktree.updated_at = Timestamp::now();
                        self.save_project_worktree_state(&worktree)?;
                        return Err(error);
                    }
                }
            }
        };

        match outcome {
            Ok(loom_vcs::MergeIntegrationOutcome::AlreadyPresent) => {
                let integration = project_child_integration_record(
                    strategy,
                    &child_revision,
                    &parent_revision,
                    &parent_revision,
                    0,
                    false,
                    Vec::new(),
                );
                self.finish_project_child_integration(&mut worktree, &parent_revision, integration)
            }
            Ok(loom_vcs::MergeIntegrationOutcome::FastForward(revision))
            | Ok(loom_vcs::MergeIntegrationOutcome::Merged(revision)) => {
                let revision = revision.to_string();
                let (commits, history_rewritten, checks) = match replay.as_ref() {
                    Some(replayed) => (
                        replayed.commits,
                        replayed.history_rewritten,
                        replayed.checks.clone(),
                    ),
                    None => (
                        project_child_integration_commit_count(
                            &child_git,
                            &parent_revision,
                            &revision,
                        ),
                        false,
                        Vec::new(),
                    ),
                };
                let integration = project_child_integration_record(
                    strategy,
                    &child_revision,
                    &parent_revision,
                    &revision,
                    commits,
                    history_rewritten,
                    checks,
                );
                self.finish_project_child_integration(&mut worktree, &revision, integration)
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

    /// Replays a finished child's own commits onto the parent tip for an
    /// integration, reusing the rebase action's bounded checks.
    ///
    /// The parent checkout is not touched here: the replay happens in the
    /// child's checkout and the caller fast-forwards the parent to the replayed
    /// revision afterwards. A conflicted replay is refused with its conflicting
    /// paths recorded and both checkouts unchanged. A replay that would touch a
    /// path the reviewed change never touched, or whose bounded check fails,
    /// restores the child's branch to the reviewed revision.
    fn replay_project_child_for_integration(
        &self,
        worktree: &mut ProjectWorktreeRecord,
        child_git: &GitService,
        parent_revision: &str,
        child_revision: &str,
    ) -> Result<ProjectChildReplayIntegration> {
        let reviewed_paths = project_child_rebase_stat_paths(child_git, &worktree.base_revision)?;
        let replayed = match child_git.rebase_onto_revision(parent_revision)? {
            loom_vcs::RebaseIntegrationOutcome::Rebased(revision) => revision.to_string(),
            loom_vcs::RebaseIntegrationOutcome::MergeRequired => {
                return Err(LoomError::invalid_state(
                    "the child's own commits contain a merge, so they cannot be replayed without dropping it; integrate it with strategy `merge` if the repository permits merge commits, or land it yourself and record it with mark_landed",
                ));
            }
            loom_vcs::RebaseIntegrationOutcome::Conflicted(paths) => {
                let mut conflict_paths = paths.clone();
                conflict_paths.sort();
                conflict_paths.dedup();
                worktree.conflict_paths = conflict_paths;
                return Err(LoomError::conflict(format!(
                    "the child's commits do not replay onto {parent_revision} cleanly; the child checkout and the parent were left unchanged, and the conflicting paths are: {}",
                    project_child_rebase_paths(paths.iter())
                )));
            }
        };
        let replay_paths = project_child_rebase_stat_paths(child_git, parent_revision)?;
        let gained = paths_outside_set(&replay_paths, &reviewed_paths);
        if !gained.is_empty() {
            self.restore_project_child_rebase(child_git, child_revision);
            return Err(LoomError::invalid_state(format!(
                "the replay would change paths the reviewed commit never touched: {}; the child checkout was restored to {child_revision}",
                project_child_rebase_paths(gained.iter())
            )));
        }
        let checks = self.run_project_child_rebase_checks(child_git);
        let failed = checks
            .iter()
            .filter(|check| check.outcome.is_failure())
            .map(|check| format!("{} ({})", check.name, check.outcome.as_str()))
            .collect::<Vec<_>>();
        if !failed.is_empty() {
            self.restore_project_child_rebase(child_git, child_revision);
            return Err(LoomError::conflict(format!(
                "the replayed commits did not pass their checks: {}; the child checkout was restored to {child_revision}, so nothing was integrated",
                failed.join(", ")
            )));
        }
        let commits = project_child_integration_commit_count(child_git, parent_revision, &replayed);
        // The child's branch now holds the replayed revision, so the reviewed
        // revision recorded for recovery and for the completion guard follows
        // it.
        worktree.result_revision = Some(replayed.clone());
        worktree.updated_at = Timestamp::now();
        self.save_project_worktree_state(worktree)?;
        log::info!(
            "[loom-server] integrating project child task {} by replaying {child_revision} onto {parent_revision}: {replayed}",
            worktree.task_id
        );
        Ok(ProjectChildReplayIntegration {
            revision: replayed.clone(),
            commits,
            history_rewritten: replayed != child_revision,
            checks: checks
                .iter()
                .map(|check| format!("{}: {}", check.name, check.outcome.as_str()))
                .collect(),
        })
    }

    fn finish_project_child_integration(
        &self,
        worktree: &mut ProjectWorktreeRecord,
        integrated_revision: &str,
        integration: ProjectWorktreeIntegration,
    ) -> Result<ServerResponse> {
        worktree.status = ProjectWorktreeStatus::Integrated;
        worktree.integrated_revision = Some(integrated_revision.to_owned());
        worktree.landed_by = Some(ProjectWorktreeLandedBy::Integrated);
        worktree.integration = Some(integration);
        worktree.conflict_paths.clear();
        worktree.error = None;
        worktree.updated_at = Timestamp::now();
        self.save_project_worktree_state(worktree)?;
        self.auto_archive_integrated_child(worktree.child_session_id);
        Ok(ServerResponse::Project(
            ProjectResponse::ProjectChildWorktreeUpdated(Box::new(worktree.clone())),
        ))
    }

    /// Records that the manager landed a reviewed child result itself.
    ///
    /// A manager that cherry-picks, replays, or squashes the reviewed change
    /// leaves Loom nothing to integrate, so without this the completion guard
    /// refuses a result that is already in the parent history. The named
    /// revision has to hold the whole reviewed change — every path the change
    /// touches, with the content the review gave it — so a wrong revision is
    /// refused instead of recorded. What landed is recorded with the evidence
    /// the manager gave, and the checkout is archived like an integration.
    pub(crate) fn mark_project_child_landed(
        &self,
        project_id: ProjectId,
        manager_session_id: AgentSessionId,
        task_id: loom_core::TaskId,
        landing: ProjectChildLanding,
    ) -> Result<ProjectWorktreeRecord> {
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
        let evidence = landing.evidence.trim();
        if evidence.is_empty() || evidence.len() > MAX_PROJECT_CHILD_LANDED_EVIDENCE_BYTES {
            return Err(LoomError::invalid_request(format!(
                "mark_landed requires 1 to {MAX_PROJECT_CHILD_LANDED_EVIDENCE_BYTES} bytes of evidence stating how the reviewed change was landed"
            )));
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
                "a project child can be marked landed only after its task completes",
            ));
        }
        if worktree.status == ProjectWorktreeStatus::Removed {
            return Err(LoomError::invalid_state(
                "a removed project child checkout has no result left to mark landed",
            ));
        }
        let Some(result_revision) = worktree.result_revision.clone() else {
            return Err(LoomError::invalid_state(
                "review the project child's committed result before marking it landed",
            ));
        };
        if result_revision == worktree.base_revision {
            return Err(LoomError::invalid_state(
                "project child has no committed change to mark landed",
            ));
        }
        let revision = landing.revision.trim();
        if revision.is_empty() {
            return Err(LoomError::invalid_request(
                "mark_landed requires the revision that carries the reviewed change",
            ));
        }
        let parent_git =
            self.session_git(worktree.parent_session_id, worktree.parent_repository_id)?;
        let carry = parent_git
            .change_carry(&worktree.base_revision, &result_revision, revision)
            .map_err(|error| {
                LoomError::invalid_request(format!(
                    "revision '{revision}' could not be verified against the parent repository: {}",
                    error.message
                ))
            })?;
        if carry.changed_paths.is_empty() {
            return Err(LoomError::invalid_state(
                "the reviewed result changes no paths, so no revision can be shown to carry it",
            ));
        }
        if !carry.mismatched_paths.is_empty() {
            return Err(LoomError::invalid_state(format!(
                "revision {revision} does not carry the reviewed result {result_revision}: these paths still differ from the reviewed content: {}",
                carry.mismatched_paths.join(", "),
            )));
        }

        worktree.status = ProjectWorktreeStatus::Integrated;
        worktree.result_revision = Some(result_revision);
        worktree.integrated_revision = Some(revision.to_owned());
        worktree.landed_by = Some(ProjectWorktreeLandedBy::Replay);
        worktree.landed_evidence = Some(evidence.to_owned());
        worktree.conflict_paths.clear();
        worktree.error = None;
        worktree.updated_at = Timestamp::now();
        self.save_project_worktree_state(&worktree)?;
        self.auto_archive_integrated_child(worktree.child_session_id);
        Ok(worktree)
    }

    /// Best-effort auto-archive of a code child once its result is integrated.
    /// A disabled policy or a refusal is logged and never fails integration.
    fn auto_archive_integrated_child(&self, child_session_id: AgentSessionId) {
        if let Err(error) = self.backend.auto_archive_completed_child(child_session_id) {
            log::warn!(
                "[loom-server] could not auto-archive integrated sub agent {child_session_id}: {}",
                error.message
            );
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
            return Ok(ServerResponse::Project(
                ProjectResponse::ProjectChildWorktreeUpdated(Box::new(worktree)),
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
                ProjectResponse::ProjectChildWorktreeUpdated(Box::new(worktree)),
            ));
        }

        worktree.status = ProjectWorktreeStatus::CleanupPending;
        self.save_project_worktree_state(&worktree)?;
        let force = disposition == ProjectWorktreeCleanupDisposition::DiscardChanges;
        // A forced removal discards whatever the checkout still holds, which must
        // not be silent: the removed record is the last place a manager can see
        // that this work existed and was thrown away. The record's note field is
        // used for that, exactly as the abandon path records what it left behind.
        // A checkout that cannot be inspected is only logged, because an
        // unreadable or already removed path leaves nothing to report.
        let discarded = if force {
            match project_child_worktree_status(&self.backend, &worktree) {
                Ok(status) if !status.clean => Some(status),
                Ok(_) => None,
                Err(error) => {
                    log::warn!(
                        "project child worktree '{}' could not be inspected before discarding its changes: {}",
                        worktree.worktree_name,
                        error.message
                    );
                    None
                }
            }
        } else {
            None
        };
        if let Err(error) = self
            .backend
            .remove_project_worktree_checkout(&worktree, force)
        {
            worktree.error = Some(error.message.clone());
            worktree.updated_at = Timestamp::now();
            self.save_project_worktree_state(&worktree)?;
            return Err(error);
        }
        worktree.status = ProjectWorktreeStatus::Removed;
        worktree.error = discarded.map(|status| {
            let additions = status
                .files
                .iter()
                .map(|file| file.index_additions.saturating_add(file.worktree_additions))
                .fold(0, u32::saturating_add);
            let deletions = status
                .files
                .iter()
                .map(|file| file.index_deletions.saturating_add(file.worktree_deletions))
                .fold(0, u32::saturating_add);
            format!(
                "the manager discarded this checkout, so its {} uncommitted path(s) ({} added, {} removed line(s)) will not be landed",
                status.files.len(),
                additions,
                deletions
            )
        });
        worktree.updated_at = Timestamp::now();
        persistence.save_project_worktree(&worktree)?;
        self.backend.journal()?.append_server_event(
            CURRENT_PROTOCOL_VERSION,
            worktree.parent_session_id,
            ServerEvent::ProjectChildWorktreeUpdated {
                worktree: worktree.clone(),
            },
        );
        Ok(ServerResponse::Project(
            ProjectResponse::ProjectChildWorktreeUpdated(Box::new(worktree)),
        ))
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
        self.backend.journal()?.append_server_event(
            CURRENT_PROTOCOL_VERSION,
            worktree.parent_session_id,
            ServerEvent::ProjectChildWorktreeUpdated {
                worktree: worktree.clone(),
            },
        );
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
                worktree.landed_by = Some(ProjectWorktreeLandedBy::Integrated);
                worktree.conflict_paths.clear();
                worktree.error = None;
                worktree.updated_at = Timestamp::now();
                self.save_project_worktree_state(&worktree)?;
                self.auto_archive_integrated_child(worktree.child_session_id);
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
            // Refresh the remote before the checkout is materialized, so the
            // base the child is actually created from is reported against the
            // fetched default-branch tip. A refresh recorded moments ago for
            // this same creation is reused instead of fetching twice.
            if !base_refresh_is_recent(worktree.base_refresh.as_ref(), Timestamp::now()) {
                worktree.base_refresh =
                    Some(self.refresh_checkout_base_revision(&parent_git, &worktree.base_revision));
                worktree.updated_at = Timestamp::now();
            }
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

/// What one replay of a child's own commits contributed to an integration.
struct ProjectChildReplayIntegration {
    /// The revision the child's branch holds after the replay.
    revision: String,
    commits: u32,
    history_rewritten: bool,
    /// One bounded line per check the rebase ran or skipped.
    checks: Vec<String>,
}

/// The worktree state a refused integration leaves behind.
///
/// A conflict is recorded as one so a later review names the paths, a checkout
/// that needs explicit attention stays recovery-required, and every other
/// refusal leaves the child ready to retry.
fn project_child_integration_refusal_status(code: ErrorCode) -> ProjectWorktreeStatus {
    match code {
        ErrorCode::Conflict => ProjectWorktreeStatus::Conflict,
        ErrorCode::RecoveryRequired => ProjectWorktreeStatus::RecoveryRequired,
        _ => ProjectWorktreeStatus::Ready,
    }
}

/// The record of one integration, named by the strategy it used and the
/// revisions it moved between.
fn project_child_integration_record(
    strategy: ProjectWorktreeIntegrationStrategy,
    previous_revision: &str,
    parent_revision: &str,
    integrated_revision: &str,
    commits: u32,
    history_rewritten: bool,
    checks: Vec<String>,
) -> ProjectWorktreeIntegration {
    ProjectWorktreeIntegration {
        strategy,
        previous_revision: previous_revision.to_owned(),
        parent_revision: parent_revision.to_owned(),
        integrated_revision: integrated_revision.to_owned(),
        commits,
        history_rewritten,
        checks,
        integrated_at: Timestamp::now(),
    }
}

/// How many commits an integration landed.
///
/// A count that cannot be read is reported as zero with a warning instead of
/// failing an integration the parent already accepted.
fn project_child_integration_commit_count(
    child_git: &GitService,
    parent_revision: &str,
    integrated_revision: &str,
) -> u32 {
    match child_git.patch_commits_since(parent_revision, integrated_revision) {
        Ok(commits) => u32::try_from(commits.len()).unwrap_or(u32::MAX),
        Err(error) => {
            log::warn!(
                "could not count the commits an integration landed from {parent_revision} to {integrated_revision}: {}",
                error.message
            );
            0
        }
    }
}

/// Most bytes of a repository's own instructions that the merge policy reads.
const MAX_REPOSITORY_POLICY_BYTES: u64 = 64 * 1024;

/// Longest instruction line the merge policy reads, so one minified line cannot
/// make the check expensive.
const MAX_REPOSITORY_POLICY_LINE_BYTES: usize = 240;

/// Why the parent checkout's own instructions forbid merge commits, if they do.
///
/// The detection is deliberately narrow: it reads only `AGENTS.md` at the
/// checkout root, and only a line that names a `merge commit` and a prohibition
/// (`no`, `never`, `do not`, `must not`, `forbid`, or `without`). A repository
/// that states the rule some other way, or names no `AGENTS.md`, is treated as
/// permitting a merge, so this never invents a rule and only refuses a merge the
/// repository itself forbids. The line is returned so a refusal can name it.
fn merge_commits_forbidden(parent_root: &Path) -> Option<String> {
    let path = parent_root.join("AGENTS.md");
    let metadata = fs::metadata(&path).ok()?;
    if !metadata.is_file() || metadata.len() > MAX_REPOSITORY_POLICY_BYTES {
        return None;
    }
    let instructions = fs::read_to_string(&path).ok()?;
    merge_commit_prohibition(&instructions)
}

/// The first instruction line in `instructions` that forbids merge commits.
fn merge_commit_prohibition(instructions: &str) -> Option<String> {
    instructions
        .lines()
        .map(str::trim)
        .map(|line| line.trim_start_matches(['-', '*']).trim())
        .find(|line| line.len() <= MAX_REPOSITORY_POLICY_LINE_BYTES && forbids_merge_commits(line))
        .map(str::to_owned)
}

/// Whether one instruction line forbids merge commits.
fn forbids_merge_commits(line: &str) -> bool {
    let line = line.to_ascii_lowercase();
    if !line.contains("merge commit") {
        return false;
    }
    [
        "no merge commit",
        "never",
        "do not",
        "don't",
        "must not",
        "forbid",
        "without merge",
    ]
    .iter()
    .any(|prohibition| line.contains(prohibition))
}

/// Message for an integration that has to create a commit of its own.
///
/// The child's own message describes the change, so the commit carries it
/// together with a trailer naming the task, instead of a bookkeeping subject
/// that says nothing about what landed. A child with no message of its own
/// falls back to a conventional subject that describes the integration.
fn project_child_integration_message(
    child_git: &GitService,
    child_revision: &str,
    task: &loom_core::DelegatedTaskRecord,
) -> String {
    let trailer = format!("Loom-Task: {}", task.task_id);
    let child_message = match child_git.commit_message(child_revision) {
        Ok(message) => message,
        Err(error) => {
            log::warn!(
                "could not read the message of project child task {} revision {child_revision}: {}",
                task.task_id,
                error.message,
            );
            String::new()
        }
    };
    let child_message = child_message.trim();
    if child_message.is_empty() {
        format!(
            "chore(project): integrate code child {} ({})\n\n{trailer}",
            task.child_name, task.task_id
        )
    } else {
        format!("{child_message}\n\n{trailer}")
    }
}

#[cfg(test)]
mod tests {
    use super::{forbids_merge_commits, merge_commit_prohibition};

    #[test]
    fn the_merge_policy_reads_only_a_prohibition_that_names_merge_commits() {
        assert_eq!(
            merge_commit_prohibition(
                "# Agent instructions\n\n- Do not create merge commits. Keep history linear.\n"
            ),
            Some("Do not create merge commits. Keep history linear.".to_owned())
        );
        assert_eq!(
            merge_commit_prohibition("Never add a merge commit.\n"),
            Some("Never add a merge commit.".to_owned())
        );
        // A line that mentions a merge without forbidding one states no rule,
        // and neither does an instruction about something else.
        for instructions in [
            "",
            "Merge commits are described here.",
            "Do not create bookkeeping commits.",
            "History stays linear.",
        ] {
            assert_eq!(
                merge_commit_prohibition(instructions),
                None,
                "{instructions:?}"
            );
        }
        assert!(!forbids_merge_commits("merge commits are welcome"));
        assert!(forbids_merge_commits("A merge commit is forbidden here"));
        assert!(forbids_merge_commits("land it without merge commits"));

        // One minified line cannot make the policy check expensive.
        let long = format!("Do not create merge commits. {}", "x".repeat(240));
        assert_eq!(merge_commit_prohibition(&long), None);
    }
}
