use super::*;

impl LoomView {
    /// Loads the review projections through the connection worker.
    pub(crate) fn refresh_review(&mut self, cx: &mut Context<Self>) {
        self.review.repositories_loaded = false;
        self.dispatch(
            cx,
            ClientRequest::Filesystem(FilesystemRequest::ListSessionDirectories {
                session_id: self.active_session.id,
            }),
            |view, response, _| match response.result {
                Ok(ServerResponse::Filesystem(FilesystemResponse::SessionDirectories {
                    directories,
                })) => {
                    view.session_directories = directories;
                }
                Err(error) => view.record_backend_error("list session directories", error),
                Ok(response) => view.record_backend_error(
                    "list session directories",
                    unexpected_response("session directory list", response),
                ),
            },
        );
        let session_id = self.active_session.id;
        self.dispatch(
            cx,
            ClientRequest::Filesystem(FilesystemRequest::GetSessionFilesystemChanges {
                session_id,
                after_sequence: None,
            }),
            move |view, response, _| {
                if view.active_session.id != session_id {
                    return;
                }
                match response.result {
                    Ok(ServerResponse::Filesystem(
                        FilesystemResponse::SessionFilesystemChanges { changes, truncated },
                    )) => {
                        let mut seen = BTreeSet::new();
                        view.review.changes = changes
                            .into_iter()
                            .rev()
                            .filter(|change| seen.insert(change.path.clone()))
                            .take(MAX_REVIEW_CHANGES)
                            .collect();
                        if truncated {
                            view.record_status(
                                "Workspace review is showing the most recent changes".to_owned(),
                            );
                        }
                    }
                    Err(error) => view.record_backend_error("workspace review refresh", error),
                    Ok(response) => view.record_backend_error(
                        "workspace review refresh",
                        unexpected_response("workspace changes", response),
                    ),
                }
            },
        );
        let session_id = self.active_session.id;
        self.dispatch(
            cx,
            ClientRequest::Repository(RepositoryRequest::ListSessionRepositories { session_id }),
            move |view, response, cx| {
                if view.active_session.id != session_id {
                    return;
                }
                match response.result {
                    Ok(ServerResponse::Repository(RepositoryResponse::SessionRepositories {
                        repositories,
                    })) => {
                        view.review.repositories_loaded = true;
                        let repository = repositories
                            .iter()
                            .find(|repository| Some(repository.id) == view.selected_repository_id)
                            .or_else(|| repositories.first())
                            .cloned();
                        view.session_repositories = repositories;
                        view.selected_repository_id =
                            repository.as_ref().map(|repository| repository.id);
                        if let Some(repository) = repository {
                            let repository_id = repository.id;
                            let session_id = view.active_session.id;
                            view.dispatch(
                                cx,
                                ClientRequest::Repository(RepositoryRequest::GetSessionVcsStatus {
                                    session_id,
                                    repository_id,
                                }),
                                move |view, response, _| {
                                    if view.active_session.id != session_id
                                        || view.selected_repository_id != Some(repository_id)
                                    {
                                        return;
                                    }
                                    match response.result {
                                        Ok(ServerResponse::Repository(
                                            RepositoryResponse::VcsStatus(status),
                                        )) => view.review.vcs = Some(status),
                                        Err(error) => {
                                            view.review.vcs = None;
                                            view.record_status(format!(
                                                "VCS review unavailable: {error}"
                                            ));
                                        }
                                        Ok(response) => view.record_backend_error(
                                            "VCS review refresh",
                                            unexpected_response("VCS status", response),
                                        ),
                                    }
                                },
                            );
                        } else {
                            view.review.vcs = None;
                        }
                    }
                    Err(error) => {
                        view.review.repositories_loaded = true;
                        view.review.vcs = None;
                        view.record_status(format!("VCS review unavailable: {error}"));
                    }
                    Ok(response) => {
                        view.review.repositories_loaded = true;
                        view.record_backend_error(
                            "VCS review refresh",
                            unexpected_response("session repository list", response),
                        );
                    }
                }
            },
        );
    }

    pub(crate) fn toggle_review_pane(&mut self, cx: &mut Context<Self>) {
        self.review.open = !self.review.open;
        if self.review.open {
            self.refresh_review(cx);
        }
        cx.notify();
    }

    pub(crate) fn close_review(&mut self, _: &ClickEvent, _: &mut Window, cx: &mut Context<Self>) {
        self.review.open = false;
        self.project_child_review = None;
        self.session_drawer_open = false;
        cx.notify();
    }

    pub(crate) fn open_review_file(&mut self, path: String, cx: &mut Context<Self>) {
        self.project_child_review = None;
        self.review.selected_path = Some(path.clone());
        self.review.selection_revision += 1;
        let selection_revision = self.review.selection_revision;
        self.review.selected_staged = false;
        self.review.selected_diff = None;
        self.review.rows.clear();
        self.review.hunk_rows.clear();
        self.review.list_state.reset(0);
        self.review.loading_diff = true;
        self.review.diff_error = None;
        let session_id = self.active_session.id;
        let requested_path = path.clone();
        self.dispatch(
            cx,
            ClientRequest::Filesystem(FilesystemRequest::ReadSessionFile { session_id, path }),
            move |view, response, _| {
                if view.active_session.id != session_id
                    || view.review.selected_path.as_deref() != Some(&requested_path)
                    || view.review.selected_staged
                    || view.review.selection_revision != selection_revision
                {
                    return;
                }
                match response.result {
                    Ok(ServerResponse::Filesystem(FilesystemResponse::SessionFilesystemFile(
                        mut file,
                    ))) => {
                        file.content = bounded_to(&file.content, MAX_REVIEW_DIFF);
                        view.review.selected_file = Some(loom_protocol::SessionFilesystemFile {
                            session_id: file.session_id,
                            path: file.path,
                            content: file.content,
                            revision: file.revision,
                        });
                        view.review.open = true;
                        view.review.panel = ReviewPanel::Changes;
                        view.review.loading_diff = false;
                    }
                    Err(error) => {
                        view.review.loading_diff = false;
                        view.review.diff_error = Some(error.to_string());
                    }
                    Ok(response) => {
                        view.review.loading_diff = false;
                        view.review.diff_error =
                            Some(unexpected_response("workspace file", response).to_string());
                    }
                }
            },
        );
    }

    pub(crate) fn jump_review_hunk(&mut self, forward: bool, cx: &mut Context<Self>) {
        if self.review.hunk_rows.is_empty() {
            return;
        }
        let visible_row = self.review.list_state.logical_scroll_top().item_ix;
        self.review.selected_hunk = if forward {
            self.review
                .hunk_rows
                .iter()
                .position(|row| *row > visible_row)
                .unwrap_or(self.review.hunk_rows.len() - 1)
        } else {
            self.review
                .hunk_rows
                .iter()
                .rposition(|row| *row < visible_row)
                .unwrap_or(0)
        };
        self.review.list_state.scroll_to(gpui_kit::ListOffset {
            item_ix: self.review.hunk_rows[self.review.selected_hunk],
            offset_in_item: px(0.),
        });
        cx.notify();
    }

    pub(crate) fn open_review_diff(&mut self, path: String, staged: bool, cx: &mut Context<Self>) {
        let Some(repository_id) = self.selected_repository_id else {
            return;
        };
        self.project_child_review = None;
        self.review.selected_path = Some(path.clone());
        self.review.selection_revision += 1;
        let selection_revision = self.review.selection_revision;
        self.review.selected_staged = staged;
        self.review.selected_file = None;
        self.review.selected_diff = None;
        self.review.rows.clear();
        self.review.hunk_rows.clear();
        self.review.list_state.reset(0);
        self.review.loading_diff = true;
        self.review.diff_error = None;
        let session_id = self.active_session.id;
        self.dispatch(
            cx,
            ClientRequest::Repository(RepositoryRequest::GetSessionVcsDiff {
                session_id,
                repository_id,
                path: Some(path.clone()),
                staged,
            }),
            move |view, response, _| {
                if view.active_session.id != session_id
                    || view.review.selected_path.as_deref() != Some(&path)
                    || view.review.selected_staged != staged
                    || view.review.selection_revision != selection_revision
                {
                    return;
                }
                match response.result {
                    Ok(ServerResponse::Repository(RepositoryResponse::VcsDiff(diff))) => {
                        view.review.show_diff(diff)
                    }
                    Err(error) => {
                        view.review.loading_diff = false;
                        view.review.diff_error = Some(error.to_string());
                    }
                    Ok(response) => {
                        view.review.loading_diff = false;
                        view.review.diff_error =
                            Some(unexpected_response("VCS diff", response).to_string());
                    }
                }
            },
        );
        cx.notify();
    }

    pub(crate) fn toggle_tool(&mut self, tool_id: ToolCallId, cx: &mut Context<Self>) {
        if !self.expanded_tools.remove(&tool_id) {
            self.expanded_tools.insert(tool_id);
        }
        cx.notify();
    }

    pub(crate) fn toggle_tool_group(&mut self, key: u64, cx: &mut Context<Self>) {
        if !self.expanded_tool_groups.remove(&key) {
            self.expanded_tool_groups.insert(key);
        }
        cx.notify();
    }

    pub(crate) fn toggle_reasoning(&mut self, key: u64, cx: &mut Context<Self>) {
        if !self.expanded_reasoning.remove(&key) {
            self.expanded_reasoning.insert(key);
        }
        cx.notify();
    }
}
