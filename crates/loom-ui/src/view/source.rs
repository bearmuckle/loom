use super::*;

impl LoomView {
    pub(crate) fn begin_source_dialog(
        &mut self,
        purpose: SessionSourceDialogPurpose,
        cx: &mut Context<Self>,
    ) {
        let active_node_id = self.session_node_ids.get(&self.active_session.id);
        let local_directory_available = local_source_available(
            purpose,
            self.local_directory_sources_available,
            active_node_id.map(String::as_str),
            &self.default_backend_node_id,
        );
        let choice = source_dialog_initial_state(purpose, local_directory_available);
        self.source_dialog = Some(SessionSourceDialog {
            purpose,
            choice,
            local_directory_available,
            filter_subscription: None,
            repositories: Vec::new(),
            selected_repository: None,
            repositories_loading: false,
            error: None,
        });
        self.source_path_input = None;
        self.repository_filter_input = None;
        self.pending_source_path = None;
        if choice == SessionSourceChoice::GitHub {
            self.load_github_repositories(cx);
        }
        cx.notify();
    }

    pub(crate) fn choose_source(&mut self, choice: SessionSourceChoice, cx: &mut Context<Self>) {
        let Some(dialog) = self.source_dialog.as_ref() else {
            return;
        };
        if !source_choice_is_allowed(dialog.purpose, dialog.local_directory_available, choice) {
            return;
        }
        if let Some(dialog) = &mut self.source_dialog {
            dialog.choice = choice;
            dialog.error = None;
            if choice == SessionSourceChoice::GitHub && dialog.repositories.is_empty() {
                dialog.repositories_loading = true;
            }
        }
        if choice == SessionSourceChoice::GitHub {
            self.load_github_repositories(cx);
        }
        cx.notify();
    }

    pub(crate) fn browse_local_directory(&mut self, cx: &mut Context<Self>) {
        let receiver = cx.prompt_for_paths(gpui_kit::PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some("Choose a local folder".into()),
        });
        cx.spawn(async move |view, cx| match receiver.await {
            Ok(Ok(Some(paths))) => {
                if let Some(path) = paths.into_iter().next() {
                    view.update(cx, |view, cx| {
                        view.pending_source_path = Some(path.display().to_string());
                        cx.notify();
                    })
                    .ok();
                }
            }
            Ok(Ok(None)) => {}
            Ok(Err(error)) => {
                view.update(cx, |view, cx| {
                    view.record_status(format!("Could not open folder browser: {error}"));
                    cx.notify();
                })
                .ok();
            }
            Err(_) => {}
        })
        .detach();
    }

    pub(crate) fn load_github_repositories(&mut self, cx: &mut Context<Self>) {
        let node_id = self
            .source_dialog
            .as_ref()
            .map(|dialog| match dialog.purpose {
                SessionSourceDialogPurpose::StartSession => self.default_backend_node_id.clone(),
                SessionSourceDialogPurpose::AddToSession => self
                    .session_node_ids
                    .get(&self.active_session.id)
                    .cloned()
                    .unwrap_or_else(|| self.default_backend_node_id.clone()),
            });
        let Some(node_id) = node_id else {
            return;
        };
        if let Some(dialog) = &mut self.source_dialog {
            dialog.repositories_loading = true;
            dialog.error = None;
        }
        self.dispatch_to_node(
            cx,
            node_id,
            ClientRequest::Repository(RepositoryRequest::ListGitHubRepositories),
            |view, response, _| {
                if let Some(dialog) = &mut view.source_dialog {
                    dialog.repositories_loading = false;
                    match response.result {
                        Ok(ServerResponse::Repository(
                            RepositoryResponse::GitHubRepositories { repositories },
                        )) => {
                            dialog.repositories = repositories;
                            dialog.error = None;
                        }
                        Err(error) => dialog.error = Some(error.message),
                        Ok(response) => {
                            dialog.error = Some(
                                unexpected_response("GitHub repository list", response).message,
                            )
                        }
                    }
                }
            },
        );
    }

    pub(crate) fn confirm_source_dialog(&mut self, cx: &mut Context<Self>) {
        let Some(dialog) = self.source_dialog.take() else {
            return;
        };
        let local_path = self
            .source_path_input
            .as_ref()
            .map(|input| input.read(cx).value().to_string())
            .unwrap_or_default();
        let source = match dialog.choice {
            SessionSourceChoice::Empty => None,
            SessionSourceChoice::LocalDirectory => {
                let path = local_path.trim();
                if path.is_empty() || !PathBuf::from(path).is_absolute() {
                    self.source_dialog = Some(dialog);
                    self.record_status("Enter an absolute local directory path");
                    return;
                }
                Some(SessionCreationSource::LocalDirectory(path.to_owned()))
            }
            SessionSourceChoice::GitHub => {
                let selected_repository = dialog.selected_repository.as_deref();
                let Some(repository) = dialog
                    .repositories
                    .iter()
                    .find(|repository| selected_repository == Some(repository.full_name.as_str()))
                    .cloned()
                else {
                    self.source_dialog = Some(dialog);
                    self.record_status("Choose a GitHub repository");
                    return;
                };
                Some(SessionCreationSource::GitHub(repository))
            }
        };
        self.source_dialog = None;
        self.source_path_input = None;
        self.repository_filter_input = None;
        match dialog.purpose {
            SessionSourceDialogPurpose::StartSession => {
                let name = source.as_ref().map_or_else(
                    || format!("Project {}", self.sessions.len().saturating_add(1)),
                    session_name_for_source,
                );
                self.create_session_on_node_with_source(
                    self.default_backend_node_id.clone(),
                    name,
                    source,
                    cx,
                );
            }
            SessionSourceDialogPurpose::AddToSession => {
                if let Some(source) = source {
                    self.add_source_to_active_session(source, cx);
                }
            }
        }
    }

    pub(crate) fn add_source_to_active_session(
        &mut self,
        source: SessionCreationSource,
        cx: &mut Context<Self>,
    ) {
        let session_id = self.active_session.id;
        let existing_mounts = self
            .session_directories
            .iter()
            .map(|directory| directory.path.clone())
            .chain(
                self.session_repositories
                    .iter()
                    .map(|repository| repository.path.clone()),
            )
            .collect::<Vec<_>>();
        match source {
            SessionCreationSource::LocalDirectory(source) => {
                let path = source_mount_path("sources", &source, &existing_mounts);
                self.dispatch(
                    cx,
                    ClientRequest::Filesystem(FilesystemRequest::AttachSessionDirectory {
                        session_id,
                        source,
                        path,
                    }),
                    |view, response, cx| match response.result {
                        Ok(ServerResponse::Filesystem(
                            FilesystemResponse::SessionDirectoryAttached {
                                directory,
                                repositories,
                            },
                        )) => {
                            view.session_directories.push(directory);
                            if let Some(repository) = repositories.first() {
                                view.selected_repository_id = Some(repository.id);
                            }
                            view.session_repositories.extend(repositories);
                            view.refresh_review(cx);
                            cx.notify();
                        }
                        Err(error) => view.record_backend_error("attach directory", error),
                        Ok(response) => view.record_backend_error(
                            "attach directory",
                            unexpected_response("directory attachment", response),
                        ),
                    },
                );
            }
            SessionCreationSource::GitHub(repository) => {
                let path =
                    source_mount_path("repositories", &repository.full_name, &existing_mounts);
                self.dispatch(
                    cx,
                    ClientRequest::Repository(RepositoryRequest::AttachSessionRepository {
                        session_id,
                        source: repository.clone_url,
                        path,
                        revision: None,
                    }),
                    move |view, response, cx| match response.result {
                        Ok(ServerResponse::Repository(
                            RepositoryResponse::SessionRepositoryAttached(repository),
                        )) => {
                            view.selected_repository_id = Some(repository.id);
                            view.session_repositories.push(repository);
                            view.refresh_review(cx);
                        }
                        Err(error) => view.record_backend_error("attach repository", error),
                        Ok(response) => view.record_backend_error(
                            "attach repository",
                            unexpected_response("repository attachment", response),
                        ),
                    },
                );
            }
        }
    }
}
