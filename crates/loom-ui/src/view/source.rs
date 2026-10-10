use super::*;

/// Debounce window before a repository search is sent, so typing does not
/// issue a request per keystroke against the rate-limited GitHub search API.
const REPOSITORY_SEARCH_DEBOUNCE_MS: u64 = 250;

impl LoomView {
    pub(crate) fn begin_source_dialog(
        &mut self,
        purpose: SessionSourceDialogPurpose,
        cx: &mut Context<Self>,
    ) {
        let target_node_id = match purpose {
            SessionSourceDialogPurpose::StartSession => self.default_backend_node_id.clone(),
            SessionSourceDialogPurpose::AddToSession => self
                .session_node_ids
                .get(&self.active_session.id)
                .cloned()
                .unwrap_or_else(|| self.default_backend_node_id.clone()),
        };
        let local_directory_available = local_source_available(
            self.local_directory_sources_available,
            &target_node_id,
            &self.default_backend_node_id,
        );
        let choice = source_dialog_initial_state(purpose, local_directory_available);
        self.source_dialog = Some(SessionSourceDialog {
            purpose,
            target_node_id,
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
        self.cloned_repositories = Vec::new();
        self.cloned_repositories_loading = false;
        self.repository_search_generation = self.repository_search_generation.wrapping_add(1);
        self.repository_search_query.clear();
        if choice == SessionSourceChoice::LocalDirectory {
            self.pending_source_path = self
                .local_current_directory
                .as_ref()
                .map(|directory| directory.display().to_string());
        }
        if choice == SessionSourceChoice::GitHub {
            self.load_cloned_repositories(cx);
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
        }
        if choice == SessionSourceChoice::LocalDirectory {
            self.prefill_source_directory_if_empty(cx);
        }
        if choice == SessionSourceChoice::GitHub && self.cloned_repositories.is_empty() {
            self.load_cloned_repositories(cx);
        }
        cx.notify();
    }

    /// Pre-fills the local-directory path with the native worker's working
    /// directory, unless the user has already entered a path.
    fn prefill_source_directory_if_empty(&mut self, cx: &Context<Self>) {
        let Some(directory) = self
            .local_current_directory
            .as_ref()
            .map(|directory| directory.display().to_string())
        else {
            return;
        };
        let already_set = self
            .source_path_input
            .as_ref()
            .is_some_and(|input| !input.read(cx).value().trim().is_empty());
        if !already_set {
            self.pending_source_path = Some(directory);
        }
    }

    /// Replaces the local-directory path with the native worker's working
    /// directory, for the "Use current folder" action.
    pub(crate) fn use_current_source_directory(&mut self, cx: &mut Context<Self>) {
        let Some(directory) = self
            .local_current_directory
            .as_ref()
            .map(|directory| directory.display().to_string())
        else {
            return;
        };
        self.pending_source_path = Some(directory);
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

    /// The worker node the source dialog targets.
    pub(crate) fn source_node_id(&self) -> Option<String> {
        self.source_dialog
            .as_ref()
            .map(|dialog| dialog.target_node_id.clone())
    }

    /// Switches the worker the source dialog creates on. Repository results and
    /// clone caches belong to the previous worker, so they are cleared and the
    /// new worker's clones are loaded when relevant.
    pub(crate) fn choose_source_node(&mut self, node_id: String, cx: &mut Context<Self>) {
        let Some(dialog) = self.source_dialog.as_ref() else {
            return;
        };
        if dialog.target_node_id == node_id {
            return;
        }
        let local_directory_available = local_source_available(
            self.local_directory_sources_available,
            &node_id,
            &self.default_backend_node_id,
        );
        if let Some(dialog) = &mut self.source_dialog {
            dialog.target_node_id = node_id;
            dialog.local_directory_available = local_directory_available;
            dialog.error = None;
            dialog.selected_repository = None;
            dialog.repositories.clear();
            dialog.repositories_loading = false;
            if !source_choice_is_allowed(dialog.purpose, local_directory_available, dialog.choice) {
                dialog.choice =
                    source_dialog_initial_state(dialog.purpose, local_directory_available);
            }
        }
        self.cloned_repositories.clear();
        self.cloned_repositories_loading = false;
        self.repository_search_generation = self.repository_search_generation.wrapping_add(1);
        self.repository_search_query.clear();
        if self
            .source_dialog
            .as_ref()
            .is_some_and(|dialog| dialog.choice == SessionSourceChoice::GitHub)
        {
            self.load_cloned_repositories(cx);
        }
        cx.notify();
    }

    /// Loads the repositories already cloned on the worker node so the picker
    /// can offer an existing clone without any search.
    pub(crate) fn load_cloned_repositories(&mut self, cx: &mut Context<Self>) {
        let Some(node_id) = self.source_node_id() else {
            return;
        };
        self.cloned_repositories_loading = true;
        self.dispatch_to_node(
            cx,
            node_id,
            ClientRequest::Repository(RepositoryRequest::ListClonedRepositories),
            |view, response, _| {
                view.cloned_repositories_loading = false;
                if let Ok(ServerResponse::Repository(RepositoryResponse::ClonedRepositories {
                    repositories,
                })) = response.result
                {
                    view.cloned_repositories = repositories;
                }
            },
        );
    }

    /// Handles a repository filter change. Queries shorter than the minimum are
    /// cleared locally and never reach the worker, and searches are debounced.
    pub(crate) fn on_repository_search_changed(&mut self, cx: &mut Context<Self>) {
        let query = self
            .repository_filter_input
            .as_ref()
            .map(|input| input.read(cx).value().trim().to_owned())
            .unwrap_or_default();
        self.repository_search_generation = self.repository_search_generation.wrapping_add(1);
        let generation = self.repository_search_generation;
        if query.chars().count() < GITHUB_REPOSITORY_QUERY_MIN_CHARS {
            self.repository_search_query.clear();
            if let Some(dialog) = &mut self.source_dialog {
                dialog.repositories.clear();
                dialog.repositories_loading = false;
                dialog.error = None;
            }
            cx.notify();
            return;
        }
        if query == self.repository_search_query {
            return;
        }
        cx.spawn(async move |view, cx| {
            #[cfg(target_family = "wasm")]
            {
                let promise = js_sys::Promise::new(&mut |resolve, _reject| {
                    if let Some(window) = web_sys::window() {
                        let _ = window.set_timeout_with_callback_and_timeout_and_arguments_0(
                            &resolve,
                            REPOSITORY_SEARCH_DEBOUNCE_MS as i32,
                        );
                    }
                });
                let _ = wasm_bindgen_futures::JsFuture::from(promise).await;
            }
            #[cfg(not(target_family = "wasm"))]
            {
                cx.background_spawn(async {
                    std::thread::sleep(std::time::Duration::from_millis(
                        REPOSITORY_SEARCH_DEBOUNCE_MS,
                    ));
                })
                .await;
            }
            view.update(cx, |view, cx| {
                if view.repository_search_generation == generation {
                    view.search_github_repositories(query, cx);
                }
            })
            .ok();
        })
        .detach();
    }

    /// Issues a GitHub repository search. The worker validates the minimum
    /// length and the GitHub API performs the filtering.
    pub(crate) fn search_github_repositories(&mut self, query: String, cx: &mut Context<Self>) {
        let Some(node_id) = self.source_node_id() else {
            return;
        };
        self.repository_search_generation = self.repository_search_generation.wrapping_add(1);
        let generation = self.repository_search_generation;
        self.repository_search_query = query.clone();
        if let Some(dialog) = &mut self.source_dialog {
            dialog.repositories_loading = true;
            dialog.error = None;
        }
        cx.notify();
        self.dispatch_to_node(
            cx,
            node_id,
            ClientRequest::Repository(RepositoryRequest::SearchGitHubRepositories { query }),
            move |view, response, _| {
                if view.repository_search_generation != generation {
                    return;
                }
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
                                unexpected_response("GitHub repository search", response).message,
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
                match resolve_local_source_path(
                    local_path.as_str(),
                    self.local_current_directory.as_deref(),
                ) {
                    Some(path) => Some(SessionCreationSource::LocalDirectory(path)),
                    None => {
                        self.source_dialog = Some(dialog);
                        self.record_status("Enter an absolute local directory path");
                        return;
                    }
                }
            }
            SessionSourceChoice::GitHub => {
                let selected = dialog.selected_repository.as_deref();
                let cached = selected.and_then(|full_name| {
                    self.cloned_repositories
                        .iter()
                        .find(|repository| repository.full_name == full_name)
                });
                let source = if let Some(repository) = cached {
                    GitHubSource {
                        full_name: repository.full_name.clone(),
                        clone_url: repository.clone_url.clone(),
                        reuse_local: true,
                    }
                } else if let Some(repository) = dialog
                    .repositories
                    .iter()
                    .find(|repository| selected == Some(repository.full_name.as_str()))
                {
                    GitHubSource {
                        full_name: repository.full_name.clone(),
                        clone_url: repository.clone_url.clone(),
                        reuse_local: false,
                    }
                } else {
                    self.source_dialog = Some(dialog);
                    self.record_status("Choose a GitHub repository");
                    return;
                };
                Some(SessionCreationSource::GitHub(source))
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
                    dialog.target_node_id.clone(),
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
            SessionCreationSource::GitHub(source) => {
                let path = source_mount_path("repositories", &source.full_name, &existing_mounts);
                self.dispatch(
                    cx,
                    ClientRequest::Repository(RepositoryRequest::AttachSessionRepository {
                        session_id,
                        source: source.clone_url,
                        path,
                        revision: None,
                        reuse_local: source.reuse_local,
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
