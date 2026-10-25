use super::*;

impl InProcessBackend {
    /// Returns an archived session to `Idle`, together with the archived
    /// descendants a project root recorded when it was archived.
    ///
    /// Nothing is removed: the transcript, checkpoints, files, and checkouts
    /// stay in place, and the session becomes eligible for a new run instead of
    /// the retention sweep. A tree whose recorded member was swept cannot be
    /// restored whole, so the request is refused and names the missing ids
    /// before any state changes. The archive gate keeps a restore mutually
    /// exclusive with the sweep and a deletion.
    pub(crate) fn restore_agent_session(
        &self,
        session_id: AgentSessionId,
    ) -> Result<ServerResponse> {
        let _gate = self.archive_gate()?;
        if self.sessions()?.get(session_id)?.state != AgentSessionState::Archived {
            return Err(LoomError::invalid_state(
                "only archived agent sessions can be restored",
            ));
        }
        let project = match &self.persistence {
            Some(persistence) => persistence.load_project_snapshot_for_session(session_id)?,
            None => None,
        };
        let is_root = project
            .as_ref()
            .is_some_and(|project| project.root_session_id == session_id);
        let mut members = match &self.persistence {
            Some(persistence) => persistence.load_archived_project_members(session_id)?,
            None => Vec::new(),
        };
        if members.is_empty()
            && is_root
            && let Some(project) = &project
        {
            // A tree archived before its membership was recorded restores the
            // archived descendants the project snapshot still names.
            let catalog = self.sessions()?;
            members = project
                .agents
                .iter()
                .filter(|agent| agent.session_id != session_id)
                .filter(|agent| {
                    catalog
                        .get(agent.session_id)
                        .is_ok_and(|session| session.state == AgentSessionState::Archived)
                })
                .map(|agent| (agent.session_id, agent.depth))
                .collect();
        }
        let mut missing = Vec::new();
        let mut restorable = Vec::new();
        {
            let catalog = self.sessions()?;
            for (member_id, depth) in members {
                match catalog.get(member_id) {
                    Ok(member) if member.state == AgentSessionState::Archived => {
                        restorable.push((member_id, depth));
                    }
                    // A member restored on its own is left as it is.
                    Ok(_) => {}
                    Err(error) if error.code == ErrorCode::NotFound => missing.push(member_id),
                    Err(error) => return Err(error),
                }
            }
        }
        if !missing.is_empty() {
            missing.sort_unstable();
            let ids = missing
                .iter()
                .map(AgentSessionId::to_string)
                .collect::<Vec<_>>()
                .join(", ");
            log::warn!(
                "[loom-server] refusing to restore session {session_id}: archived descendant session(s) {ids} no longer exist"
            );
            return Err(LoomError::new(
                ErrorCode::InvalidState,
                format!(
                    "cannot restore session {session_id}: archived descendant session(s) {ids} were deleted"
                ),
                false,
            ));
        }
        // Deepest first, like archiving, so a parent is never restored before a
        // child that is still archived.
        restorable.sort_by_key(|(member_id, depth)| (std::cmp::Reverse(*depth), *member_id));
        for (member_id, depth) in restorable {
            let (_, record) = self.sessions()?.restore_archived(member_id)?;
            self.journal()?.append_session(record);
            log::info!(
                "[loom-server] restored descendant session {member_id} (depth {depth}) of project root {session_id}"
            );
        }
        let (snapshot, record) = self.sessions()?.restore_archived(session_id)?;
        self.journal()?.append_session(record);
        if is_root && let Some(persistence) = &self.persistence {
            persistence.clear_archived_project_members(session_id)?;
        }
        log::info!("[loom-server] restored session {session_id}");
        Ok(ServerResponse::Session(
            SessionResponse::AgentSessionRestored(snapshot),
        ))
    }

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
    /// A request that targets a project root cascades to its whole hierarchy,
    /// archived or not, once every child task is terminal and no session being
    /// deleted has a run that is loaded and live in this process; the caller
    /// cancels a live project before this runs, so a project never has to finish
    /// first. Worktrees are removed deepest-first before any row or filesystem
    /// root is touched, so a dirty or locked checkout aborts the request without
    /// changing state. The archive gate keeps this mutually exclusive with the
    /// retention sweep.
    pub(crate) fn delete_agent_session(
        &self,
        session_id: AgentSessionId,
        force: bool,
    ) -> Result<ServerResponse> {
        let _gate = self.archive_gate()?;
        let (_, response) = self.delete_agent_session_locked(session_id, force)?;
        Ok(response)
    }

    /// [`InProcessBackend::delete_agent_session`] assuming the archive gate is
    /// already held, reporting the sessions it removed.
    fn delete_agent_session_locked(
        &self,
        session_id: AgentSessionId,
        force: bool,
    ) -> Result<(BTreeSet<AgentSessionId>, ServerResponse)> {
        let session = self.sessions()?.get(session_id)?;
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
        // A project root is the only session that may be deleted without being
        // archived, because its live work is cancelled before this runs. Every
        // other session, including a project descendant, must be archived. A
        // session is trivially a one-member project of its own, so a root is a
        // project root only when it owns another member or a delegated task.
        let project_root = project.as_ref().filter(|project| {
            project.root_session_id == session_id
                && (project.agents.len() > 1 || !project.tasks.is_empty())
        });
        if project_root.is_none() && session.state != AgentSessionState::Archived {
            return Err(LoomError::invalid_state(
                "only archived agent sessions can be deleted",
            ));
        }
        let mut deleted_sessions = BTreeMap::from([(session_id, session.workspace_id)]);
        if let Some(project) = project_root {
            // Validate every child task before mutating anything, so a rejected
            // project cannot leave a partially deleted tree behind. A project
            // whose caller did not cancel it first refuses here while any task
            // is still queued, blocked, running, or otherwise unsettled.
            let unfinished_tasks = project
                .tasks
                .iter()
                .filter(|task| !delegated_task_is_terminal(task.status))
                .map(|task| format!("{} ({:?})", task.child_name, task.status))
                .collect::<Vec<_>>();
            if !unfinished_tasks.is_empty() {
                log::warn!(
                    "[loom-server] refusing to delete project root {session_id}: unfinished child tasks {unfinished_tasks:?}"
                );
                // A task committed after the caller cancelled the project lands
                // here. The caller can retry: a retried request re-reads the
                // project and cancels the newer task before it deletes.
                return Err(LoomError::new(
                    ErrorCode::InvalidState,
                    "finish or cancel every child task before deleting this project",
                    true,
                ));
            }
            // The whole tree is one unit, so every descendant is deleted with
            // its root whatever state it is in.
            for agent in &project.agents {
                if agent.session_id == session_id {
                    continue;
                }
                let child = self.sessions()?.get(agent.session_id)?;
                deleted_sessions.insert(child.id, child.workspace_id);
            }
            log::info!(
                "[loom-server] deleting project root {session_id} with {} descendant session(s)",
                deleted_sessions.len().saturating_sub(1)
            );
        }
        let deleted_ids = deleted_sessions.keys().copied().collect::<BTreeSet<_>>();
        // A loaded run that is still live would keep writing the state this
        // deletion drops, so every session being deleted must be free of
        // non-terminal runs loaded in this process. A cancelled project
        // satisfies this because the caller interrupted each of its runs first.
        // A durable run summary without a loaded handle is inert, see
        // `non_terminal_runs_of_sessions`.
        let live_runs = self.non_terminal_runs_of_sessions(&deleted_ids)?;
        if !live_runs.is_empty() {
            log::warn!(
                "[loom-server] refusing to delete session {session_id}: non-terminal runs {live_runs:?}"
            );
            return Err(LoomError::invalid_state(
                "stop or cancel every run of this session before deleting it",
            ));
        }
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
        // Build output is reproducible and belongs to a session that no longer
        // exists, so reclaim it with the checkouts.
        for id in &deleted_ids {
            self.remove_build_cache(*id);
        }
        // Drop every in-memory trace before the durable rows, so neither the
        // state save that follows this request nor a later request can observe
        // or re-insert a deleted session. The catalog accepts a removal in any
        // state: a project root is often live here, its work was cancelled above,
        // and synthesizing an archive transition for it would append journal
        // records that misreport the state of a session that no longer exists.
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
            // high-water mark so retained cursors stay valid, and retained
            // events are indexed per session, so deleting a session drops its
            // whole window.
            journal.events.retain(|id, _| !deleted_ids.contains(id));
            journal.rosters.retain(|id, _| !deleted_ids.contains(id));
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

    /// The non-terminal runs of the given sessions that are loaded in this
    /// process.
    ///
    /// Only a loaded run has an executor that can keep writing the state a
    /// deletion drops, so only a loaded run may refuse the deletion. A durable
    /// run summary without a loaded handle has no executor: refusing on one would
    /// make the session permanently undeletable, for example an archived session
    /// left by a crash between the archive write and the run checkpoint, which
    /// the retention sweep would then skip forever.
    fn non_terminal_runs_of_sessions(
        &self,
        sessions: &BTreeSet<AgentSessionId>,
    ) -> Result<Vec<String>> {
        let mut live = Vec::new();
        for (run_id, handle) in self.runs()?.iter() {
            if !sessions.contains(&handle.session_id) {
                continue;
            }
            let state = handle.snapshot().state;
            if !is_terminal_agent_run_state(state) {
                live.push(format!("{run_id} ({state:?})"));
            }
        }
        Ok(live)
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
        let window = policy.retention();
        let sub_agent_window = policy.sub_agent_retention();
        if window.is_none() && sub_agent_window.is_none() {
            log::debug!("[loom-server] archive retention sweep is idle: retention is disabled");
            return Ok(ArchiveSweepReport::default());
        }
        let _gate = self.archive_gate()?;
        let now = Timestamp::now().as_unix_millis();
        let window_ms = window.map(|window| u64::try_from(window.as_millis()).unwrap_or(u64::MAX));
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
        log::debug!(
            "[loom-server] archive retention sweep considering {} archived session(s) with a {window_ms:?} ms window and a {sub_agent_window:?} sub-agent window (force discard: {force})",
            candidates.len(),
        );

        let mut report = ArchiveSweepReport::default();
        let mut deleted = BTreeSet::new();
        if let Some(sub_agent_window) = sub_agent_window {
            let sub_agent_ms = u64::try_from(sub_agent_window.as_millis()).unwrap_or(u64::MAX);
            self.sweep_archived_sub_agents(
                &candidates,
                &catalog,
                now,
                sub_agent_ms,
                force,
                &mut deleted,
                &mut report,
            )?;
        }
        let Some(window_ms) = window_ms else {
            log::info!(
                "[loom-server] archive retention sweep deleted {} session(s) and skipped {} candidate(s)",
                report.deleted_sessions.len(),
                report.skipped.len()
            );
            return Ok(report);
        };
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
            match self.delete_agent_session_locked(session_id, force) {
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

    /// Deletes archived sub-agent sessions that are past the sub-agent window,
    /// one at a time and deepest first, so a session is only removed once its own
    /// children are gone. A sub-agent with an unfinished task or a child that is
    /// still around is left for a later round.
    #[allow(clippy::too_many_arguments)]
    fn sweep_archived_sub_agents(
        &self,
        candidates: &[AgentSessionId],
        catalog: &BTreeMap<AgentSessionId, AgentSessionSnapshot>,
        now: u64,
        sub_agent_ms: u64,
        force: bool,
        deleted: &mut BTreeSet<AgentSessionId>,
        report: &mut ArchiveSweepReport,
    ) -> Result<()> {
        let Some(persistence) = &self.persistence else {
            return Ok(());
        };
        let mut sub_agents = Vec::new();
        for session_id in candidates {
            let Some(project) = persistence.load_project_snapshot_for_session(*session_id)? else {
                continue;
            };
            let Some(agent) = project
                .agents
                .iter()
                .find(|agent| agent.session_id == *session_id)
            else {
                continue;
            };
            if project.root_session_id == *session_id {
                continue;
            }
            sub_agents.push((std::cmp::Reverse(agent.depth), *session_id));
        }
        sub_agents.sort();
        for (_, session_id) in sub_agents {
            if deleted.contains(&session_id) {
                continue;
            }
            // Re-read: an earlier deletion in this round changed the project.
            let Some(project) = persistence.load_project_snapshot_for_session(session_id)? else {
                continue;
            };
            let blocker = catalog
                .get(&session_id)
                .and_then(|session| retention_blocker_for_session(session, now, sub_agent_ms))
                .or_else(|| sub_agent_structure_blocker(&project, session_id));
            if let Some(blocker) = blocker {
                if blocker.warning {
                    log::warn!(
                        "[loom-server] archive retention skipped sub-agent {session_id}: {}",
                        blocker.reason
                    );
                } else {
                    log::info!(
                        "[loom-server] archive retention skipped sub-agent {session_id}: {}",
                        blocker.reason
                    );
                }
                report.skipped.push(ArchiveSweepSkip {
                    session_id,
                    reason: blocker.reason,
                });
                continue;
            }
            match self.delete_agent_session_locked(session_id, force) {
                Ok((removed, _)) => {
                    deleted.extend(removed.iter().copied());
                    report.deleted_sessions.extend(removed);
                }
                Err(error) => {
                    log::warn!(
                        "[loom-server] archive retention skipped sub-agent {session_id}: {}",
                        error.message
                    );
                    report.skipped.push(ArchiveSweepSkip {
                        session_id,
                        reason: error.message,
                    });
                }
            }
        }
        Ok(())
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

/// Why a project member cannot be deleted on its own yet: it still has child
/// sessions in the project or a delegated task that has not finished.
fn sub_agent_structure_blocker(
    project: &ProjectSnapshot,
    session_id: AgentSessionId,
) -> Option<RetentionBlocker> {
    if project
        .agents
        .iter()
        .any(|agent| agent.parent_session_id == Some(session_id))
    {
        return Some(RetentionBlocker::ineligible(format!(
            "sub-agent {session_id} still has child sessions"
        )));
    }
    project
        .tasks
        .iter()
        .find(|task| {
            (task.target_session_id == session_id || task.requester_session_id == session_id)
                && !delegated_task_is_terminal(task.status)
        })
        .map(|task| {
            RetentionBlocker::ineligible(format!(
                "project task '{}' is not terminal ({:?})",
                task.child_name, task.status
            ))
        })
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
    // An archive time ahead of the current clock (a stepped clock, a restored
    // snapshot) is not provably older than the window, so the session waits for
    // a later sweep instead of being discarded as soon as it is seen.
    let Some(age) = now.checked_sub(session.updated_at.as_unix_millis()) else {
        return Some(RetentionBlocker::inside_window(format!(
            "session {} has an archive time ahead of the current clock",
            session.id
        )));
    };
    if age >= window_ms {
        return None;
    }
    Some(RetentionBlocker::inside_window(format!(
        "session {} was archived {age} ms ago, inside the {window_ms} ms retention window",
        session.id
    )))
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

#[cfg(test)]
mod retention_eligibility_tests {
    use super::*;

    fn archived_session(archived_at: u64) -> AgentSessionSnapshot {
        AgentSessionSnapshot {
            id: AgentSessionId::new(),
            workspace_id: WorkspaceId::new(),
            name: "Archived".to_owned(),
            goal: None,
            state: AgentSessionState::Archived,
            created_at: Timestamp::from_unix_millis(archived_at),
            updated_at: Timestamp::from_unix_millis(archived_at),
            title_source: SessionTitleSource::Auto,
        }
    }

    #[test]
    fn only_an_archived_session_past_the_window_is_eligible() {
        let now = 1_700_000_000_000u64;
        let window_ms = 86_400_000u64;

        let expired = archived_session(now - window_ms);
        assert!(retention_blocker_for_session(&expired, now, window_ms).is_none());

        let inside = archived_session(now - window_ms + 1);
        let blocker = retention_blocker_for_session(&inside, now, window_ms)
            .expect("a session inside the window waits");
        assert!(blocker.reason.contains("inside the"));
        assert!(!blocker.warning, "waiting out the window is routine");

        let active = AgentSessionSnapshot {
            state: AgentSessionState::Idle,
            ..archived_session(now)
        };
        let blocker = retention_blocker_for_session(&active, now, window_ms)
            .expect("a session that is not archived is never eligible");
        assert!(blocker.warning);
    }

    #[test]
    fn an_archive_time_ahead_of_the_clock_waits_instead_of_deleting() {
        let now = 1_700_000_000_000u64;
        let ahead = archived_session(now + 1_000);

        let blocker = retention_blocker_for_session(&ahead, now, 1)
            .expect("a future archive time is not provably older than the window");
        assert!(blocker.reason.contains("ahead of the current clock"));
        assert!(!blocker.warning);
    }
}
