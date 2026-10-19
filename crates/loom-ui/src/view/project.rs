use super::*;

/// Copies each project agent's authoritative `state` onto the matching session
/// snapshot, returning how many entries actually changed.
///
/// A project snapshot is polled on its own cadence while the session list is
/// only rebuilt by a full reload. Without this, a delegated agent that has since
/// finished keeps whatever state the list held when it was first loaded, so its
/// project row keeps reporting activity until the client is reloaded.
///
/// The session named by `skip` is left alone: its state is owned by its own
/// event stream, which a possibly older snapshot must not regress. Sessions the
/// snapshot does not mention are untouched, and the snapshot adds no entries.
pub(crate) fn apply_project_agent_states(
    sessions: &mut [AgentSessionSnapshot],
    project: &loom_core::ProjectSnapshot,
    skip: AgentSessionId,
) -> usize {
    let mut changed = 0;
    for agent in &project.agents {
        if agent.session_id == skip {
            continue;
        }
        let Some(session) = sessions
            .iter_mut()
            .find(|session| session.id == agent.session_id)
        else {
            continue;
        };
        if session.state != agent.state {
            session.state = agent.state;
            changed += 1;
        }
    }
    changed
}

impl LoomView {
    pub(crate) fn refresh_active_project_snapshot(&mut self, cx: &mut Context<Self>) {
        let session_id = self.active_session.id;
        if self.project_snapshot_refreshing.is_some() {
            // Do not clear invalidations received while the request is pending.
            // This also queues a refresh for the latest selection after a switch.
            self.project_snapshot_stale = true;
            return;
        }
        let backend = match self.backend_for_session(session_id) {
            Ok(backend) => backend,
            Err(error) => {
                self.record_backend_error("load project snapshot", error);
                return;
            }
        };
        self.project_snapshot_stale = false;
        self.project_snapshot_refreshing = Some(session_id);
        let request = backend.submit(RequestEnvelope::new(ClientRequest::Project(
            ProjectRequest::GetProjectSnapshotForSession { session_id },
        )));
        cx.spawn(async move |view, cx| {
            let response = cx
                .background_spawn(async move { request.wait().await })
                .await;
            view.update(cx, |view, cx| {
                view.project_snapshot_refreshing = None;
                if view.active_session.id != session_id {
                    view.refresh_active_project_snapshot(cx);
                    return;
                }
                let mut reload_sessions = false;
                match response.result {
                    Ok(ServerResponse::Project(ProjectResponse::ProjectSnapshot(snapshot))) => {
                        reload_sessions =
                            project_snapshot_has_unloaded_agent_sessions(&snapshot, &view.sessions);
                        // The snapshot is authoritative for the agents it
                        // carries, so apply their states instead of waiting for
                        // a reload to notice that work has finished. The active
                        // session is skipped because its own event stream owns
                        // its state.
                        apply_project_agent_states(&mut view.sessions, &snapshot, session_id);
                        view.project_tree_snapshots
                            .retain(|known| known.project_id != snapshot.project_id);
                        view.project_tree_snapshots.push(snapshot.clone());
                        view.project_snapshot = Some(snapshot);
                    }
                    Err(error) if error.code == ErrorCode::NotFound => {
                        view.project_snapshot = None;
                    }
                    Err(error) => {
                        view.record_backend_error("load project snapshot", error);
                    }
                    Ok(response) => view.record_backend_error(
                        "load project snapshot",
                        unexpected_response("project snapshot", response),
                    ),
                }
                if reload_sessions {
                    view.reload_sessions(cx);
                }
                // Only a new invalidation queues another request; an error by
                // itself must not cause an immediate retry loop.
                if view.project_snapshot_stale {
                    view.refresh_active_project_snapshot(cx);
                }
                view.schedule_project_poll(cx);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Loads project snapshots for sessions not covered by a known project, so
    /// the sidebar nests delegated sub-tasks even for projects the user has not
    /// opened yet.
    ///
    /// Requests are issued one project at a time: a returned snapshot covers all
    /// of its agent sessions, and sessions that become covered are skipped.
    pub(crate) fn refresh_missing_project_snapshots(&mut self, cx: &mut Context<Self>) {
        if self.missing_project_snapshots_refreshing {
            return;
        }
        self.missing_project_snapshots_refreshing = true;
        cx.spawn(async move |view, cx| {
            // A reload can change the session list while a request is pending.
            // Re-read it after every response, but attempt each session only
            // once in this scan, including standalone sessions and failures.
            let mut attempted = std::collections::HashSet::new();
            loop {
                let next = view.update(cx, |view, _| {
                    let next = uncovered_session_ids(&view.sessions, &view.project_tree_snapshots)
                        .into_iter()
                        .filter(|session_id| !attempted.contains(session_id))
                        .find_map(|session_id| {
                            let node_id = view.session_node_ids.get(&session_id)?;
                            let backend = view.node_backends.get(node_id)?.clone();
                            Some((session_id, backend))
                        });
                    if next.is_none() {
                        view.missing_project_snapshots_refreshing = false;
                    }
                    next
                });
                let Ok(Some((session_id, backend))) = next else {
                    break;
                };
                attempted.insert(session_id);
                let request = backend.submit(RequestEnvelope::new(ClientRequest::Project(
                    ProjectRequest::GetProjectSnapshotForSession { session_id },
                )));
                let response = cx
                    .background_spawn(async move { request.wait().await })
                    .await;
                view.update(cx, |view, cx| {
                    if let Ok(ServerResponse::Project(ProjectResponse::ProjectSnapshot(snapshot))) =
                        response.result
                    {
                        // Even without a project open in the pane, the snapshot
                        // updates the state of every session it covers.
                        apply_project_agent_states(
                            &mut view.sessions,
                            &snapshot,
                            view.active_session.id,
                        );
                        view.project_tree_snapshots
                            .retain(|known| known.project_id != snapshot.project_id);
                        view.project_tree_snapshots.push(snapshot);
                        cx.notify();
                    }
                })
                .ok();
            }
        })
        .detach();
    }

    pub(crate) fn project_root_is_active(&self) -> bool {
        self.project_snapshot
            .as_ref()
            .is_some_and(|snapshot| snapshot.root_session_id == self.active_session.id)
    }

    pub(crate) fn project_has_live_children(&self) -> bool {
        let Some(project) = self.project_snapshot.as_ref() else {
            return false;
        };
        let has_live_task = project.tasks.iter().any(|task| {
            !matches!(
                task.status,
                loom_core::DelegatedTaskStatus::Completed
                    | loom_core::DelegatedTaskStatus::Failed
                    | loom_core::DelegatedTaskStatus::Cancelled
            )
        });
        let has_live_session = project.agents.iter().any(|agent| {
            agent.session_id != project.root_session_id
                && matches!(
                    agent.state,
                    AgentSessionState::Queued
                        | AgentSessionState::Planning
                        | AgentSessionState::AwaitingApproval
                        | AgentSessionState::Paused
                        | AgentSessionState::Executing
                        | AgentSessionState::Evaluating
                        | AgentSessionState::NeedsInput
                )
        });
        has_live_task || has_live_session
    }

    pub(crate) fn control_project_child_from_ui(
        &mut self,
        manager_session_id: AgentSessionId,
        project_id: loom_core::ProjectId,
        task_id: loom_core::TaskId,
        action: ProjectChildControlAction,
        cx: &mut Context<Self>,
    ) {
        self.dispatch(
            cx,
            ClientRequest::Project(ProjectRequest::ControlProjectChild {
                project_id,
                manager_session_id,
                task_id,
                action,
            }),
            move |view, response, cx| match response.result {
                Ok(ServerResponse::Project(ProjectResponse::ProjectChildControlled {
                    task,
                    ..
                })) => {
                    view.record_status(format!("{} · {:?}", task.child_name, task.status));
                    view.project_snapshot_stale = true;
                    view.refresh_active_project_snapshot(cx);
                }
                Err(error) => view.record_backend_error("control project child", error),
                Ok(response) => view.record_backend_error(
                    "control project child",
                    unexpected_response("project child control", response),
                ),
            },
        );
    }

    pub(crate) fn review_project_child_from_ui(
        &mut self,
        manager_session_id: AgentSessionId,
        project_id: loom_core::ProjectId,
        task_id: loom_core::TaskId,
        cx: &mut Context<Self>,
    ) {
        self.dispatch(
            cx,
            ClientRequest::Project(ProjectRequest::GetProjectChildReview {
                project_id,
                manager_session_id,
                task_id,
            }),
            move |view, response, cx| match response.result {
                Ok(ServerResponse::Project(ProjectResponse::ProjectChildReview {
                    worktree,
                    status,
                    diff,
                })) => {
                    view.project_child_review =
                        Some((worktree.as_ref().clone(), status.clone(), diff.clone()));
                    view.project_snapshot_stale = true;
                    view.review.open = true;
                    view.review.tab = InspectorTab::Changes;
                    view.review.clear_unread(InspectorTab::Changes);
                    view.review.selected_file = None;
                    view.review.selected_path = Some(format!(
                        "{} · diff from {}",
                        worktree.branch_name, worktree.base_revision
                    ));
                    view.review.selected_staged = false;
                    view.review.selection_revision = view.review.selection_revision.wrapping_add(1);
                    view.review.vcs = Some(status);
                    view.review.show_diff(diff, cx);
                    view.refresh_active_project_snapshot(cx);
                }
                Err(error) => view.record_backend_error("review project child", error),
                Ok(response) => view.record_backend_error(
                    "review project child",
                    unexpected_response("project child review", response),
                ),
            },
        );
    }

    pub(crate) fn integrate_project_child_from_ui(
        &mut self,
        manager_session_id: AgentSessionId,
        project_id: loom_core::ProjectId,
        task_id: loom_core::TaskId,
        expected_parent_revision: String,
        expected_child_revision: String,
        cx: &mut Context<Self>,
    ) {
        self.dispatch(
            cx,
            ClientRequest::Project(ProjectRequest::GetProjectChildReview{
                project_id,
                manager_session_id,
                task_id,
            }),
            move |view, response, cx| match response.result {
                Ok(ServerResponse::Project(ProjectResponse::ProjectChildReview{ status, .. })) if status.clean
                    && status.head.as_deref() == Some(expected_child_revision.as_str()) =>
                {
                    view.submit_project_child_integration(
                        manager_session_id,
                        project_id,
                        task_id,
                        expected_parent_revision,
                        cx,
                    );
                }
                Ok(ServerResponse::Project(ProjectResponse::ProjectChildReview{
                    worktree,
                    status,
                    diff,
                })) => {
                    view.project_child_review =
                        Some((worktree.as_ref().clone(), status.clone(), diff.clone()));
                    view.project_snapshot_stale = true;
                    view.review.open = true;
                    view.review.tab = InspectorTab::Changes;
                    view.review.clear_unread(InspectorTab::Changes);
                    view.review.selected_file = None;
                    view.review.selected_path = Some(format!(
                        "{} · diff from {}",
                        worktree.branch_name, worktree.base_revision
                    ));
                    view.review.selected_staged = false;
                    view.review.selection_revision = view.review.selection_revision.wrapping_add(1);
                    view.review.vcs = Some(status);
                    view.review.show_diff(diff, cx);
                    view.record_status(
                        "Child checkout changed since review or has uncommitted edits. Review the current diff before integrating.",
                    );
                    view.refresh_active_project_snapshot(cx);
                }
                Err(error) => view.record_backend_error("check child review before integration", error),
                Ok(response) => view.record_backend_error(
                    "check child review before integration",
                    unexpected_response("project child review", response),
                ),
            },
        );
    }

    pub(crate) fn submit_project_child_integration(
        &mut self,
        manager_session_id: AgentSessionId,
        project_id: loom_core::ProjectId,
        task_id: loom_core::TaskId,
        expected_parent_revision: String,
        cx: &mut Context<Self>,
    ) {
        self.dispatch(
            cx,
            ClientRequest::Project(ProjectRequest::IntegrateProjectChild {
                project_id,
                manager_session_id,
                task_id,
                expected_parent_revision,
            }),
            move |view, response, cx| match response.result {
                Ok(ServerResponse::Project(ProjectResponse::ProjectChildWorktreeUpdated(
                    worktree,
                ))) => {
                    view.project_child_review = None;
                    view.project_snapshot_stale = true;
                    view.record_status(format!(
                        "Child changes integrated at {}",
                        worktree
                            .integrated_revision
                            .as_deref()
                            .unwrap_or("unknown revision")
                    ));
                    view.refresh_active_project_snapshot(cx);
                    view.refresh_review(cx);
                }
                Err(error) => view.record_backend_error("integrate project child", error),
                Ok(response) => view.record_backend_error(
                    "integrate project child",
                    unexpected_response("project child integration", response),
                ),
            },
        );
    }

    pub(crate) fn cleanup_project_child_from_ui(
        &mut self,
        manager_session_id: AgentSessionId,
        project_id: loom_core::ProjectId,
        task_id: loom_core::TaskId,
        disposition: loom_core::ProjectWorktreeCleanupDisposition,
        cx: &mut Context<Self>,
    ) {
        self.dispatch(
            cx,
            ClientRequest::Project(ProjectRequest::CleanupProjectChildWorktree {
                project_id,
                manager_session_id,
                task_id,
                disposition,
            }),
            move |view, response, cx| match response.result {
                Ok(ServerResponse::Project(ProjectResponse::ProjectChildWorktreeUpdated(
                    worktree,
                ))) => {
                    view.project_snapshot_stale = true;
                    view.record_status(format!("Child checkout is {:?}", worktree.status));
                    if view
                        .project_child_review
                        .as_ref()
                        .is_some_and(|(current, _, _)| current.task_id == worktree.task_id)
                    {
                        view.project_child_review = None;
                        view.review.open = false;
                    }
                    view.refresh_active_project_snapshot(cx);
                }
                Err(error) => view.record_backend_error("clean up project child", error),
                Ok(response) => view.record_backend_error(
                    "clean up project child",
                    unexpected_response("project child cleanup", response),
                ),
            },
        );
    }

    pub(crate) fn build_project_session_context_menu(
        mut menu: PopupMenu,
        session: AgentSessionSnapshot,
        menu_project: Option<loom_core::ProjectSnapshot>,
        menu_view: Entity<LoomView>,
    ) -> PopupMenu {
        let rename_view = menu_view.clone();
        let rename_session = session.clone();
        // A project root carries the lifecycle actions. A delegated sub agent is
        // managed by its manager, so it only offers opening its transcript here;
        // its archive is automatic and its controls are manager-scoped.
        let is_project = menu_project
            .as_ref()
            .is_none_or(|project| project.root_session_id == session.id);
        let archive_label = menu_project
            .as_ref()
            .filter(|project| project.root_session_id == session.id)
            .map_or("Archive", |_| "Archive project");
        // Deleting a project removes the whole tree, so it is offered only for
        // the root and always goes through the confirmation.
        let is_project_root = menu_project
            .as_ref()
            .is_some_and(|project| project.root_session_id == session.id);
        if is_project {
            let archive_view = menu_view.clone();
            let archive_session = session.clone();
            menu = menu
                .item(PopupMenuItem::new("Rename").on_click(move |_, window, cx| {
                    let rename_session = rename_session.clone();
                    rename_view.update(cx, |view, cx| {
                        view.begin_session_rename(rename_session, true, window, cx);
                    });
                }))
                .item(PopupMenuItem::new(archive_label).on_click(move |_, _, cx| {
                    archive_view.update(cx, |view, cx| {
                        view.archive_session(archive_session.id, cx);
                    });
                }));
        } else {
            let open_view = menu_view.clone();
            let open_session = session.clone();
            menu = menu.item(
                PopupMenuItem::new("Open transcript").on_click(move |_, _, cx| {
                    let open_session = open_session.clone();
                    open_view.update(cx, |view, cx| {
                        view.session_drawer_open = false;
                        if view.active_session.id != open_session.id {
                            view.select_session(open_session, cx);
                        } else {
                            view.dismiss_surfaces();
                        }
                        cx.notify();
                    });
                }),
            );
        }
        // A project root can pin its title (or hand it back to the backend)
        // without renaming: the label follows who chose the current name. A
        // delegated sub agent has no title of its own, so it offers neither.
        if is_project_root {
            let (title_label, title_source) = match session.title_source {
                loom_core::SessionTitleSource::Auto => (
                    "Stop automatic titles",
                    loom_core::SessionTitleSource::Manual,
                ),
                loom_core::SessionTitleSource::Manual => (
                    "Resume automatic titles",
                    loom_core::SessionTitleSource::Auto,
                ),
            };
            let title_view = menu_view.clone();
            let title_session_id = session.id;
            menu = menu.item(PopupMenuItem::new(title_label).on_click(move |_, _, cx| {
                title_view.update(cx, |view, cx| {
                    view.set_session_title_source(title_session_id, title_source, cx);
                });
            }));
        }
        if let Some(project) = menu_project.as_ref()
            && let Some(child) = project.agents.iter().find(|agent| {
                agent.session_id == session.id
                    && agent.parent_session_id.is_some_and(|parent_session_id| {
                        parent_session_id == project.root_session_id
                            || project
                                .agents
                                .iter()
                                .any(|manager| manager.session_id == parent_session_id)
                    })
            })
            && let Some(manager_session_id) = child.parent_session_id
            && let Some(task) = project
                .tasks
                .iter()
                .find(|task| task.target_session_id == child.session_id)
        {
            let project_id = project.project_id;
            let manager_permissions = if manager_session_id == project.root_session_id {
                Some((true, true, true))
            } else {
                project
                    .tasks
                    .iter()
                    .find(|manager_task| manager_task.target_session_id == manager_session_id)
                    .map(|manager_task| {
                        (
                            manager_task.permissions.child_control,
                            manager_task.permissions.review,
                            manager_task.permissions.integration,
                        )
                    })
            };
            let (can_control, can_review, can_integrate) = manager_permissions.unwrap_or_default();
            if can_control {
                for action in project_child_control_actions(child.state, task.status) {
                    let control_view = menu_view.clone();
                    let task_id = task.task_id;
                    let action_label = if action == ProjectChildControlAction::Cancel
                        && (child.state == AgentSessionState::Failed
                            || task.status == loom_core::DelegatedTaskStatus::Failed)
                    {
                        "Cancel remaining descendants"
                    } else {
                        project_child_control_label(action)
                    };
                    menu = menu.item(PopupMenuItem::new(action_label).on_click(move |_, _, cx| {
                        control_view.update(cx, |view, cx| {
                            view.control_project_child_from_ui(
                                manager_session_id,
                                project_id,
                                task_id,
                                action,
                                cx,
                            );
                        });
                    }));
                }
            }
            if let Some(worktree) = project
                .worktrees
                .iter()
                .find(|worktree| worktree.task_id == task.task_id)
            {
                let task_id = task.task_id;
                let terminal = matches!(
                    task.status,
                    loom_core::DelegatedTaskStatus::Completed
                        | loom_core::DelegatedTaskStatus::Failed
                        | loom_core::DelegatedTaskStatus::Cancelled
                );
                if task.code_change
                    && can_review
                    && !matches!(
                        worktree.status,
                        loom_core::ProjectWorktreeStatus::CleanupPending
                            | loom_core::ProjectWorktreeStatus::Removed
                    )
                {
                    let review_view = menu_view.clone();
                    menu = menu.item(PopupMenuItem::new("Review child changes").on_click(
                        move |_, _, cx| {
                            review_view.update(cx, |view, cx| {
                                view.review_project_child_from_ui(
                                    manager_session_id,
                                    project_id,
                                    task_id,
                                    cx,
                                );
                            });
                        },
                    ));
                }
                if task.code_change
                    && can_integrate
                    && terminal
                    && task.status == loom_core::DelegatedTaskStatus::Completed
                    && matches!(
                        worktree.status,
                        loom_core::ProjectWorktreeStatus::Ready
                            | loom_core::ProjectWorktreeStatus::Stale
                    )
                    && let Some(expected_child_revision) = worktree.result_revision.clone()
                {
                    let integrate_view = menu_view.clone();
                    let expected_parent_revision = worktree.base_revision.clone();
                    menu = menu.item(PopupMenuItem::new("Integrate child changes").on_click(
                        move |_, _, cx| {
                            integrate_view.update(cx, |view, cx| {
                                view.integrate_project_child_from_ui(
                                    manager_session_id,
                                    project_id,
                                    task_id,
                                    expected_parent_revision.clone(),
                                    expected_child_revision.clone(),
                                    cx,
                                );
                            });
                        },
                    ));
                }
                if terminal && worktree.status != loom_core::ProjectWorktreeStatus::Removed {
                    if worktree.status != loom_core::ProjectWorktreeStatus::Retained {
                        let retain_view = menu_view.clone();
                        menu = menu.item(PopupMenuItem::new("Keep child checkout").on_click(
                            move |_, _, cx| {
                                retain_view.update(cx, |view, cx| {
                                    view.cleanup_project_child_from_ui(
                                        manager_session_id,
                                        project_id,
                                        task_id,
                                        loom_core::ProjectWorktreeCleanupDisposition::Retain,
                                        cx,
                                    );
                                });
                            },
                        ));
                    }
                    let cleanup_view = menu_view.clone();
                    menu = menu.item(PopupMenuItem::new("Remove clean child checkout").on_click(
                        move |_, _, cx| {
                            cleanup_view.update(cx, |view, cx| {
                                view.cleanup_project_child_from_ui(
                                    manager_session_id,
                                    project_id,
                                    task_id,
                                    loom_core::ProjectWorktreeCleanupDisposition::RemoveClean,
                                    cx,
                                );
                            });
                        },
                    ));
                }
            }
        }
        if is_project_root {
            let delete_view = menu_view.clone();
            let delete_session_id = session.id;
            menu = menu.item(
                PopupMenuItem::new("Delete project").on_click(move |_, _, cx| {
                    delete_view.update(cx, |view, cx| {
                        view.begin_project_delete(delete_session_id, cx);
                    });
                }),
            );
        }
        menu
    }
}
