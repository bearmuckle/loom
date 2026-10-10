use super::*;

impl InProcessBackend {
    /// Locks the archive gate, which serializes every deletion with the
    /// retention sweep so no caller observes a partially deleted session.
    fn archive_gate(&self) -> Result<MutexGuard<'_, ()>> {
        self.archive_gate.lock().map_err(|_| {
            LoomError::new(
                ErrorCode::Internal,
                "the archive gate lock was poisoned",
                true,
            )
        })
    }

    /// Permanently removes archived agent sessions and their stored history.
    ///
    /// A request that targets an archived project root cascades to its whole
    /// archived hierarchy after every child task is terminal. Worktrees are
    /// removed deepest-first before any row or filesystem root is touched, so a
    /// dirty or locked checkout aborts the request without changing state. The
    /// archive gate keeps this mutually exclusive with the retention sweep.
    pub(crate) fn delete_archived_session(
        &self,
        session_id: AgentSessionId,
        force: bool,
    ) -> Result<ServerResponse> {
        let _gate = self.archive_gate()?;
        let (_, response) = self.delete_archived_session_locked(session_id, force)?;
        Ok(response)
    }

    /// [`InProcessBackend::delete_archived_session`] assuming the archive gate
    /// is already held, reporting the sessions it removed.
    fn delete_archived_session_locked(
        &self,
        session_id: AgentSessionId,
        force: bool,
    ) -> Result<(BTreeSet<AgentSessionId>, ServerResponse)> {
        let session = self.sessions()?.get(session_id)?;
        if session.state != AgentSessionState::Archived {
            return Err(LoomError::invalid_state(
                "only archived agent sessions can be deleted",
            ));
        }
        let project = match &self.persistence {
            Some(persistence) => persistence.load_project_snapshot_for_session(session_id)?,
            None => {
                // Without persistence there is no project snapshot, so a project
                // root is deleted as a plain session and its worktrees are not
                // visible.
                log::debug!(
                    "[loom-server] deleting session {session_id} without persistence; project cascade and worktrees are unavailable"
                );
                None
            }
        };
        let mut deleted_sessions = BTreeMap::from([(session_id, session.workspace_id)]);
        if let Some(project) = &project
            && project.root_session_id == session_id
        {
            // Validate every descendant before mutating anything, so a
            // rejected child cannot leave a partially deleted project tree
            // behind.
            let unfinished_tasks = project
                .tasks
                .iter()
                .filter(|task| !delegated_task_is_terminal(task.status))
                .map(|task| format!("{} ({:?})", task.child_name, task.status))
                .collect::<Vec<_>>();
            let mut not_archived = Vec::new();
            for agent in &project.agents {
                if agent.session_id == session_id {
                    continue;
                }
                let child = self.sessions()?.get(agent.session_id)?;
                if child.state == AgentSessionState::Archived {
                    deleted_sessions.insert(child.id, child.workspace_id);
                } else {
                    not_archived.push(format!("{} ({:?})", child.id, child.state));
                }
            }
            if !unfinished_tasks.is_empty() || !not_archived.is_empty() {
                log::warn!(
                    "[loom-server] refusing to delete project root {session_id}: unfinished child tasks {unfinished_tasks:?}, non-archived child sessions {not_archived:?}"
                );
                return Err(LoomError::new(
                    ErrorCode::InvalidState,
                    "finish or cancel every child task before deleting this project",
                    false,
                ));
            }
            log::info!(
                "[loom-server] deleting project root {session_id} with {} descendant session(s)",
                deleted_sessions.len().saturating_sub(1)
            );
        }
        let deleted_ids = deleted_sessions.keys().copied().collect::<BTreeSet<_>>();
        // Remove the checkouts of every worktree that belongs to a deleted
        // session, deepest-first, so a parent checkout is never removed before
        // a worktree that lives under it.
        let mut removed_worktrees = Vec::new();
        if let Some(project) = &project {
            let depths = project
                .agents
                .iter()
                .map(|agent| (agent.session_id, agent.depth))
                .collect::<BTreeMap<_, _>>();
            let mut worktrees = project
                .worktrees
                .iter()
                .filter(|worktree| {
                    deleted_ids.contains(&worktree.parent_session_id)
                        || deleted_ids.contains(&worktree.child_session_id)
                })
                .collect::<Vec<_>>();
            worktrees.sort_by_key(|worktree| {
                std::cmp::Reverse(
                    depths
                        .get(&worktree.parent_session_id)
                        .copied()
                        .unwrap_or_default()
                        .max(
                            depths
                                .get(&worktree.child_session_id)
                                .copied()
                                .unwrap_or_default(),
                        ),
                )
            });
            for worktree in worktrees {
                if let Err(error) = self.remove_project_worktree_checkout(worktree, force) {
                    log::warn!(
                        "[loom-server] failed to remove project worktree {} while deleting session {session_id}: {}",
                        worktree.worktree_name,
                        error.message
                    );
                    return Err(error);
                }
                removed_worktrees.push(worktree.worktree_name.clone());
            }
        }
        // Remove the per-session roots, which contain the checkouts. The shared
        // clone cache next to the roots is never touched.
        for (id, workspace_id) in &deleted_sessions {
            let root = self
                .session_root_base
                .join(workspace_id.to_string())
                .join(id.to_string());
            match fs::remove_dir_all(&root) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => {
                    log::warn!(
                        "[loom-server] could not remove the filesystem root of deleted session {id}: {error}"
                    );
                    return Err(LoomError::new(
                        ErrorCode::WorkspaceAccessDenied,
                        format!("could not remove the stored filesystem of session {id}: {error}"),
                        false,
                    ));
                }
            }
        }
        // Drop every in-memory trace before the durable rows, so neither the
        // state save that follows this request nor a later request can observe
        // or re-insert a deleted session.
        for id in &deleted_ids {
            self.sessions()?.remove(*id)?;
        }
        self.session_filesystems()?
            .retain(|id, _| !deleted_ids.contains(id));
        self.persisted_session_filesystems()?
            .retain(|id| !deleted_ids.contains(id));
        self.session_repositories()?
            .retain(|id, _| !deleted_ids.contains(id));
        self.session_vcs()?
            .retain(|(id, _), _| !deleted_ids.contains(id));
        self.session_policies()?
            .retain(|id, _| !deleted_ids.contains(id));
        self.auto_approve_actions()?
            .retain(|id, _| !deleted_ids.contains(id));
        self.runs()?
            .retain(|_, handle| !deleted_ids.contains(&handle.session_id));
        self.persisted_runs()?
            .retain(|_, summary| !deleted_ids.contains(&summary.snapshot.session_id));
        {
            let mut journal = self.journal()?;
            // A reconnecting client must never receive events for a session that
            // no longer exists. The global sequence deliberately keeps its
            // high-water mark so retained cursors stay valid.
            journal
                .events
                .retain(|event| !deleted_ids.contains(&event.session_id));
            journal
                .pending_events
                .retain(|event| !deleted_ids.contains(&event.session_id));
        }
        if let Some(persistence) = &self.persistence {
            persistence.delete_sessions(&deleted_ids)?;
        }
        log::info!(
            "[loom-server] deleted {} archived agent session(s) for {session_id}; removed {} project worktree(s)",
            deleted_ids.len(),
            removed_worktrees.len()
        );
        Ok((
            deleted_ids,
            ServerResponse::Session(SessionResponse::AgentSessionDeleted { session_id }),
        ))
    }

    /// Discards archived sessions and fully archived project trees whose archive
    /// time is older than the configured retention window.
    ///
    /// The sweep runs without a connection: it resolves its own candidates from
    /// the session catalog and the persisted project snapshots. A candidate that
    /// is not eligible yet, or that the deletion path refuses (a dirty or locked
    /// worktree, a non-terminal task), is reported and skipped so the round
    /// continues with the other candidates. `Ok` reports what the round did; an
    /// error is reserved for a failure that stops the sweep itself.
    pub fn sweep_archive_retention(&self) -> Result<ArchiveSweepReport> {
        let policy = self.archive_retention();
        let Some(window) = policy.retention() else {
            return Ok(ArchiveSweepReport::default());
        };
        let _gate = self.archive_gate()?;
        let now = Timestamp::now().as_unix_millis();
        let window_ms = u64::try_from(window.as_millis()).unwrap_or(u64::MAX);
        let force = policy.force_discard_worktrees;
        // The catalog is read once: `Archived` is terminal, so a candidate can
        // only leave it by being deleted, and the deletion path re-validates
        // every session under the same gate.
        let mut catalog = BTreeMap::new();
        for session in self.sessions()?.list_in_workspace(None, true) {
            catalog.insert(session.id, session);
        }
        let mut candidates = catalog
            .values()
            .filter(|session| session.state == AgentSessionState::Archived)
            .map(|session| session.id)
            .collect::<Vec<_>>();
        candidates.sort_unstable();

        let mut report = ArchiveSweepReport::default();
        let mut deleted = BTreeSet::new();
        for session_id in candidates {
            // A session deleted by an earlier candidate in this round is gone.
            if deleted.contains(&session_id) {
                continue;
            }
            let project = match &self.persistence {
                Some(persistence) => persistence.load_project_snapshot_for_session(session_id)?,
                None => None,
            };
            if let Some(project) = &project
                && project.root_session_id != session_id
            {
                // A descendant is deleted with its root; the root is a
                // candidate of its own when it is archived.
                log::debug!(
                    "[loom-server] archive retention leaves session {session_id} to project root {}",
                    project.root_session_id
                );
                continue;
            }
            let blocker = match &project {
                Some(project) => self.project_retention_blocker(project, &catalog, now, window_ms),
                None => catalog
                    .get(&session_id)
                    .and_then(|session| retention_blocker_for_session(session, now, window_ms)),
            };
            if let Some(blocker) = blocker {
                // A tree that is not in an eligible state is worth a warning; a
                // window that has not elapsed yet is routine, so it stays at
                // info level rather than warning on every sweep.
                if blocker.warning {
                    log::warn!(
                        "[loom-server] archive retention skipped {session_id}: {}",
                        blocker.reason
                    );
                } else {
                    log::info!(
                        "[loom-server] archive retention skipped {session_id}: {}",
                        blocker.reason
                    );
                }
                report.skipped.push(ArchiveSweepSkip {
                    session_id,
                    reason: blocker.reason,
                });
                continue;
            }
            match self.delete_archived_session_locked(session_id, force) {
                Ok((removed, _)) => {
                    deleted.extend(removed.iter().copied());
                    report.deleted_sessions.extend(removed);
                }
                Err(error) => {
                    log::warn!(
                        "[loom-server] archive retention skipped {session_id}: {}",
                        error.message
                    );
                    report.skipped.push(ArchiveSweepSkip {
                        session_id,
                        reason: error.message,
                    });
                }
            }
        }
        log::info!(
            "[loom-server] archive retention sweep deleted {} session(s) and skipped {} candidate(s)",
            report.deleted_sessions.len(),
            report.skipped.len()
        );
        Ok(report)
    }

    /// The first reason `project` cannot be discarded yet, or `None` when every
    /// session in its persisted snapshot is archived and past the window and
    /// every child task is terminal.
    fn project_retention_blocker(
        &self,
        project: &ProjectSnapshot,
        catalog: &BTreeMap<AgentSessionId, AgentSessionSnapshot>,
        now: u64,
        window_ms: u64,
    ) -> Option<RetentionBlocker> {
        for agent in &project.agents {
            let Some(session) = catalog.get(&agent.session_id) else {
                return Some(RetentionBlocker::ineligible(format!(
                    "project session {} is not loaded",
                    agent.session_id
                )));
            };
            if let Some(reason) = retention_blocker_for_session(session, now, window_ms) {
                return Some(reason);
            }
        }
        project
            .tasks
            .iter()
            .find(|task| !delegated_task_is_terminal(task.status))
            .map(|task| {
                RetentionBlocker::ineligible(format!(
                    "project task '{}' is not terminal ({:?})",
                    task.child_name, task.status
                ))
            })
    }

    /// Removes one project worktree checkout and its in-memory registrations.
    ///
    /// The checkout is resolved through the child session filesystem when it is
    /// loaded and through the session root layout otherwise, because the child
    /// session row still exists at every call site. A worktree that is locked or
    /// dirty is refused unless `force` is true, and a registration that is
    /// already gone with its checkout is treated as removed. Any other failure
    /// is returned to the caller so it can abort without touching the session
    /// rows.
    pub(crate) fn remove_project_worktree_checkout(
        &self,
        worktree: &ProjectWorktreeRecord,
        force: bool,
    ) -> Result<()> {
        let parent_git =
            self.session_git(worktree.parent_session_id, worktree.parent_repository_id)?;
        let relative_path = checked_session_relative_path(&worktree.relative_path)?;
        let child_filesystem = self
            .session_filesystems()?
            .get(&worktree.child_session_id)
            .cloned();
        let destination = match &child_filesystem {
            Some(filesystem) => filesystem.root().join(&relative_path),
            None => {
                let child = self.sessions()?.get(worktree.child_session_id)?;
                self.session_root_base
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
        self.session_repositories()?
            .entry(worktree.child_session_id)
            .or_default()
            .remove(&worktree.child_repository_id);
        self.session_vcs()?
            .remove(&(worktree.child_session_id, worktree.child_repository_id));
        if let Some(filesystem) = child_filesystem {
            filesystem.mark_state_dirty()?;
        }
        Ok(())
    }
}

/// Why the sweep left one candidate alone this round.
struct RetentionBlocker {
    reason: String,
    /// A tree that is not in an eligible state is worth a warning; a window
    /// that has not elapsed yet is routine.
    warning: bool,
}

impl RetentionBlocker {
    /// A session that is not archived yet, or a task that is not terminal.
    fn ineligible(reason: String) -> Self {
        Self {
            reason,
            warning: true,
        }
    }

    /// A tree whose archive time is still inside the retention window.
    fn inside_window(reason: String) -> Self {
        Self {
            reason,
            warning: false,
        }
    }
}

/// Whether one session is archived and past the retention window, or the reason
/// it is not.
fn retention_blocker_for_session(
    session: &AgentSessionSnapshot,
    now: u64,
    window_ms: u64,
) -> Option<RetentionBlocker> {
    if session.state != AgentSessionState::Archived {
        return Some(RetentionBlocker::ineligible(format!(
            "session {} is {:?}",
            session.id, session.state
        )));
    }
    if let Some(age) = now.checked_sub(session.updated_at.as_unix_millis()) {
        if age >= window_ms {
            return None;
        }
        return Some(RetentionBlocker::inside_window(format!(
            "session {} was archived {age} ms ago, inside the {window_ms} ms retention window",
            session.id
        )));
    }
    None
}

/// Whether a delegated task has finished, so a project holding it may be
/// discarded.
fn delegated_task_is_terminal(status: loom_core::DelegatedTaskStatus) -> bool {
    matches!(
        status,
        loom_core::DelegatedTaskStatus::Completed
            | loom_core::DelegatedTaskStatus::Failed
            | loom_core::DelegatedTaskStatus::Cancelled
    )
}
