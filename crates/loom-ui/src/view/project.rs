use super::*;

impl LoomView {
    pub(crate) fn rebuild_project_message_timeline(&mut self) {
        self.timeline
            .retain(|item| !matches!(item, TimelineItem::ProjectMessage(_)));
        if self
            .project_snapshot
            .as_ref()
            .is_some_and(|snapshot| snapshot.root_session_id == self.active_session.id)
        {
            remove_project_message_context_duplicates(&mut self.timeline, &self.project_messages);
            self.timeline.extend(
                self.project_messages
                    .iter()
                    .cloned()
                    .map(TimelineItem::ProjectMessage),
            );
        }
    }

    pub(crate) fn refresh_active_project_snapshot(&mut self, cx: &mut Context<Self>) {
        let session_id = self.active_session.id;
        let backend = match self.backend_for_session(session_id) {
            Ok(backend) => backend,
            Err(error) => {
                self.record_backend_error("load project snapshot", error);
                return;
            }
        };
        self.project_snapshot_stale = false;
        let request = backend.submit(RequestEnvelope::new(ClientRequest::Project(
            ProjectRequest::GetProjectSnapshotForSession { session_id },
        )));
        cx.spawn(async move |view, cx| {
            let response = cx
                .background_spawn(async move { request.wait().await })
                .await;
            view.update(cx, |view, cx| {
                if view.active_session.id != session_id {
                    return;
                }
                let mut reload_sessions = false;
                match response.result {
                    Ok(ServerResponse::Project(ProjectResponse::ProjectSnapshot(snapshot))) => {
                        reload_sessions =
                            project_snapshot_has_unloaded_agent_sessions(&snapshot, &view.sessions);
                        view.project_tree_snapshots
                            .retain(|known| known.project_id != snapshot.project_id);
                        view.project_tree_snapshots.push(snapshot.clone());
                        view.project_snapshot = Some(snapshot);
                        view.project_messages_stale = true;
                    }
                    Err(error) if error.code == ErrorCode::NotFound => {
                        view.project_snapshot = None;
                        view.project_messages.clear();
                        view.project_message_cursors.clear();
                        view.rebuild_project_message_timeline();
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
                view.refresh_project_messages(cx);
                view.schedule_project_poll(cx);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    pub(crate) fn refresh_project_messages(&mut self, cx: &mut Context<Self>) {
        if self.project_messages_loading || !self.project_messages_stale {
            return;
        }
        let Some(project) = self
            .project_snapshot
            .as_ref()
            .filter(|snapshot| snapshot.root_session_id == self.active_session.id)
        else {
            return;
        };
        let project_id = project.project_id;
        let mut recipients = project
            .agents
            .iter()
            .map(|agent| agent.session_id)
            .collect::<Vec<_>>();
        recipients.push(project.root_session_id);
        recipients.sort();
        recipients.dedup();
        if recipients.len() <= 1 {
            self.project_messages.clear();
            self.project_message_cursors.clear();
            self.project_messages_stale = false;
            self.rebuild_project_message_timeline();
            return;
        }
        let backend = match self.backend_for_session(self.active_session.id) {
            Ok(backend) => backend,
            Err(error) => {
                self.record_backend_error("load project messages", error);
                return;
            }
        };
        let mut cursors = self.project_message_cursors.clone();
        let generation = self.project_message_generation;
        self.project_messages_stale = false;
        self.project_messages_loading = true;
        cx.spawn(async move |view, cx| {
            let mut messages = Vec::new();
            let mut first_failure = None;
            for session_id in recipients {
                let mut cursor = cursors.get(&session_id).copied();
                loop {
                    let request = backend.submit(RequestEnvelope::new(ClientRequest::Project(
                        ProjectRequest::ListProjectAgentMessages {
                            project_id,
                            session_id,
                            after_project_sequence: cursor,
                            limit: 512,
                        },
                    )));
                    let response = cx
                        .background_spawn(async move { request.wait().await })
                        .await;
                    match response.result {
                        Ok(ServerResponse::Project(ProjectResponse::ProjectAgentMessages {
                            messages: page,
                            next_after_project_sequence,
                        })) => {
                            if let Some(last) = page.last() {
                                cursor = Some(last.project_sequence);
                                cursors.insert(session_id, last.project_sequence);
                            }
                            messages.extend(page);
                            if let Some(next) = next_after_project_sequence {
                                cursor = Some(next);
                                cursors.insert(session_id, next);
                                continue;
                            }
                            break;
                        }
                        Err(error) => {
                            if first_failure.is_none() {
                                first_failure = Some(error);
                            }
                            break;
                        }
                        Ok(response) => {
                            if first_failure.is_none() {
                                first_failure =
                                    Some(unexpected_response("project messages", response));
                            }
                            break;
                        }
                    }
                }
            }
            view.update(cx, |view, cx| {
                if view.project_message_generation != generation
                    || !view
                        .project_snapshot
                        .as_ref()
                        .is_some_and(|snapshot| view.active_session.id == snapshot.root_session_id)
                {
                    return;
                }
                view.project_messages_loading = false;
                view.project_message_cursors = cursors;
                for message in messages {
                    if !view
                        .project_messages
                        .iter()
                        .any(|existing| existing.message_id == message.message_id)
                    {
                        view.project_messages.push(message);
                    }
                }
                view.project_messages
                    .sort_by_key(|message| message.project_sequence);
                view.rebuild_project_message_timeline();
                let had_failure = first_failure.is_some();
                if let Some(error) = first_failure {
                    view.project_messages_stale = true;
                    view.record_backend_error("load project messages", error);
                }
                if !had_failure && view.project_messages_stale {
                    view.refresh_project_messages(cx);
                }
                cx.notify();
            })
            .ok();
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
                        Some((worktree.clone(), status.clone(), diff.clone()));
                    view.project_snapshot_stale = true;
                    view.review.open = true;
                    view.review.tab = InspectorTab::Changes;
                    view.review.selected_file = None;
                    view.review.selected_path = Some(format!(
                        "{} · diff from {}",
                        worktree.branch_name, worktree.base_revision
                    ));
                    view.review.selected_staged = false;
                    view.review.selection_revision = view.review.selection_revision.wrapping_add(1);
                    view.review.vcs = Some(status);
                    view.review.show_diff(diff);
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
                        Some((worktree.clone(), status.clone(), diff.clone()));
                    view.project_snapshot_stale = true;
                    view.review.open = true;
                    view.review.tab = InspectorTab::Changes;
                    view.review.selected_file = None;
                    view.review.selected_path = Some(format!(
                        "{} · diff from {}",
                        worktree.branch_name, worktree.base_revision
                    ));
                    view.review.selected_staged = false;
                    view.review.selection_revision = view.review.selection_revision.wrapping_add(1);
                    view.review.vcs = Some(status);
                    view.review.show_diff(diff);
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
        let archive_view = menu_view.clone();
        let rename_session = session.clone();
        let is_project = menu_project
            .as_ref()
            .is_none_or(|project| project.root_session_id == session.id);
        let archive_session = session.clone();
        let archive_label = menu_project
            .as_ref()
            .filter(|project| project.root_session_id == session.id)
            .map_or("Archive", |_| "Archive project");
        menu = menu
            .item(PopupMenuItem::new("Rename").on_click(move |_, window, cx| {
                let rename_session = rename_session.clone();
                rename_view.update(cx, |view, cx| {
                    view.begin_session_rename(rename_session, is_project, window, cx);
                });
            }))
            .item(PopupMenuItem::new(archive_label).on_click(move |_, _, cx| {
                archive_view.update(cx, |view, cx| {
                    view.archive_session(archive_session.id, cx);
                });
            }));
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
            if task.code_change
                && let Some(worktree) = project
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
                if can_review
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
                if can_integrate
                    && terminal
                    && task.status == loom_core::DelegatedTaskStatus::Completed
                    && worktree.status == loom_core::ProjectWorktreeStatus::Ready
                    && let Some(expected_child_revision) = worktree.result_revision.clone()
                {
                    let integrate_view = menu_view.clone();
                    let expected_parent_revision = worktree.base_revision.clone();
                    menu = menu.item(PopupMenuItem::new("Fast-forward child changes").on_click(
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
        menu
    }
}
