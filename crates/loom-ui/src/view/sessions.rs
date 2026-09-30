use super::*;

impl LoomView {
    pub(crate) fn activate_session(&mut self, session: AgentSessionSnapshot) {
        let workspace_changed = self.active_session.workspace_id != session.workspace_id;
        self.active_session = session;
        self.project_snapshot = None;
        self.project_snapshot_stale = false;
        self.project_messages.clear();
        self.project_message_cursors.clear();
        self.project_messages_stale = false;
        self.project_messages_loading = false;
        self.project_message_generation = self.project_message_generation.wrapping_add(1);
        if workspace_changed {
            self.project_feed_after_sequence = None;
            self.project_feed_epoch = None;
            self.project_poll_scheduled = false;
        }
        self.session_repositories.clear();
        self.session_directories.clear();
        self.selected_repository_id = None;
        self.session_state = self.active_session.state;
        self.auto_approve_actions = self
            .session_auto_approve_actions
            .get(&self.active_session.id)
            .copied()
            .unwrap_or(true);
        self.approval_settings_request_in_flight = false;
        self.model = self
            .session_models
            .get(&self.active_session.id)
            .cloned()
            .unwrap_or_else(|| self.default_model.clone());
        self.reset_projection();
        self.review.selected_file = None;
        self.review.selected_path = None;
        self.review.selected_staged = false;
        self.review.selected_diff = None;
        self.review.selected_file = None;
        self.review.rows.clear();
        self.review.hunk_rows.clear();
        self.review.list_state.reset(0);
        self.review.changes.clear();
        self.review.vcs = None;
        self.review.repositories_loaded = false;
        self.review.usage = UsageState::default();
        self.review.files = FilesState::default();
    }

    /// Loads a session synchronously.
    ///
    /// Only used by the startup bootstrap, before a window exists; every
    /// interactive path uses [`Self::select_session`], which goes through the
    /// connection worker.
    #[cfg(not(target_family = "wasm"))]
    pub(crate) fn load_session(&mut self, session: AgentSessionSnapshot) {
        self.activate_session(session);
        let metadata_response =
            self.connection
                .request(RequestEnvelope::new(ClientRequest::Session(
                    SessionRequest::GetAgentSessionInitialState {
                        session_id: self.active_session.id,
                    },
                )));
        let snapshot_response = if metadata_response.result.is_err() {
            self.connection
                .request(RequestEnvelope::new(ClientRequest::Session(
                    SessionRequest::GetAgentSessionSnapshot {
                        session_id: self.active_session.id,
                    },
                )))
        } else {
            metadata_response
        };
        let mut event_cursor = None;
        let snapshot_result = match snapshot_response.result {
            Ok(ServerResponse::Session(SessionResponse::AgentSessionInitialState(initial))) => {
                event_cursor = Some(initial.cursor);
                Ok(ServerResponse::Session(
                    SessionResponse::AgentSessionSnapshot(initial.projection),
                ))
            }
            result => result,
        };
        let mut needs_transcript_page = false;
        let fallback_projection = match snapshot_result {
            Ok(ServerResponse::Session(SessionResponse::AgentSessionSnapshot(projection))) => {
                needs_transcript_page = projection
                    .active_run
                    .as_ref()
                    .is_some_and(|run| run.messages.is_empty());
                self.active_session = projection.session.clone();
                self.session_state = self.active_session.state;
                self.auto_approve_actions = projection.auto_approve_actions;
                self.session_auto_approve_actions
                    .insert(self.active_session.id, projection.auto_approve_actions);
                if let Some(run) = &projection.active_run {
                    self.model = run.run.model.clone();
                    self.session_task_cache
                        .insert(self.active_session.id, run.run.task.clone());
                }
                Some(projection)
            }
            Err(error) => {
                self.record_backend_error("load session snapshot", error);
                None
            }
            Ok(response) => {
                self.record_backend_error(
                    "load session snapshot",
                    unexpected_response("session snapshot", response),
                );
                None
            }
        };
        if let Err(error) = self.collect_events_since(
            event_cursor,
            fallback_projection.and_then(|projection| projection.active_run),
        ) {
            self.record_backend_error("load session events", error);
        }
        if needs_transcript_page && let Some(run_id) = self.active_run_id {
            match load_transcript_page_sync(&self.connection, run_id, None) {
                Ok((messages, next_before, has_older)) => {
                    self.apply_transcript_page(run_id, None, messages, next_before, has_older);
                }
                Err(error) => self.record_backend_error("load conversation history", error),
            }
        }
        self.ensure_session_task_message(self.active_session.id);
        if let Some(run) = &self.active_run {
            self.session_task_cache
                .insert(self.active_session.id, run.task.clone());
            self.ensure_session_task_message(self.active_session.id);
        }
        match self
            .connection
            .request(RequestEnvelope::new(ClientRequest::Repository(
                RepositoryRequest::ListSessionRepositories {
                    session_id: self.active_session.id,
                },
            )))
            .result
        {
            Ok(ServerResponse::Repository(RepositoryResponse::SessionRepositories {
                repositories,
            })) => {
                self.selected_repository_id = repositories.first().map(|repository| repository.id);
                self.session_repositories = repositories;
            }
            Err(error) => self.record_backend_error("load session repositories", error),
            Ok(response) => self.record_backend_error(
                "load session repositories",
                unexpected_response("session repository list", response),
            ),
        }
        if let Ok(ServerResponse::Filesystem(FilesystemResponse::SessionDirectories {
            directories,
        })) = self
            .connection
            .request(RequestEnvelope::new(ClientRequest::Filesystem(
                FilesystemRequest::ListSessionDirectories {
                    session_id: self.active_session.id,
                },
            )))
            .result
        {
            self.session_directories = directories;
        }
        if let Ok(ServerResponse::Project(ProjectResponse::ProjectSnapshot(snapshot))) = self
            .connection
            .request(RequestEnvelope::new(ClientRequest::Project(
                ProjectRequest::GetProjectSnapshotForSession {
                    session_id: self.active_session.id,
                },
            )))
            .result
        {
            self.project_tree_snapshots
                .retain(|known| known.project_id != snapshot.project_id);
            self.project_tree_snapshots.push(snapshot.clone());
            self.project_snapshot = Some(snapshot);
            self.project_messages_stale = true;
        }
    }

    pub(crate) fn confirm_rename(&mut self, cx: &mut Context<Self>) {
        let Some(dialog) = self.rename_dialog.take() else {
            return;
        };
        let name = self
            .rename_input_state
            .as_ref()
            .map(|input| input.read(cx).value().to_string())
            .unwrap_or_default()
            .trim()
            .to_owned();
        if name.is_empty() {
            self.record_status("Session name cannot be empty");
            self.rename_dialog = Some(dialog);
            return;
        }
        self.dispatch(
            cx,
            ClientRequest::Session(SessionRequest::RenameAgentSession {
                session_id: dialog.session.id,
                name,
            }),
            move |view, response, cx| match response.result {
                Ok(ServerResponse::Session(SessionResponse::AgentSessionRenamed(snapshot))) => {
                    view.active_session = snapshot;
                    view.reload_sessions(cx);
                }
                Err(error) => {
                    view.rename_dialog = Some(dialog);
                    view.record_backend_error("rename session", error);
                }
                Ok(response) => view.record_backend_error(
                    "rename session",
                    unexpected_response("session rename", response),
                ),
            },
        );
    }

    /// Archives `session_id`. The target is explicit so a menu action cannot
    /// archive the previously active session if selection fails.
    pub(crate) fn archive_session(&mut self, session_id: AgentSessionId, cx: &mut Context<Self>) {
        if self.archive_request_in_flight {
            log::info!(
                "[loom-ui] ignoring archive request for {session_id}: another archive is already in flight"
            );
            return;
        }
        self.archive_request_in_flight = true;
        self.record_status("Archiving session...");
        self.dispatch(
            cx,
            ClientRequest::Session(SessionRequest::ArchiveAgentSession { session_id }),
            move |view, response, cx| {
                view.archive_request_in_flight = false;
                match response.result {
                    Ok(ServerResponse::Session(SessionResponse::AgentSessionArchived(
                        snapshot,
                    ))) => {
                        log::info!("[loom-ui] archived session {}", snapshot.id);
                        let was_active = view.active_session.id == snapshot.id;
                        view.sessions.retain(|session| session.id != snapshot.id);
                        view.session_node_ids.remove(&snapshot.id);
                        if was_active {
                            if let Some(session) = view.sessions.first().cloned() {
                                view.select_session(session, cx);
                            } else {
                                view.activate_session(empty_session_snapshot(view.workspace_id));
                                view.review.open = false;
                                cx.notify();
                            }
                        } else {
                            view.status_banner = None;
                            view.update_session_list();
                            cx.notify();
                        }
                    }
                    Err(error) => {
                        log::warn!(
                            "[loom-ui] archive session {session_id} failed: {:?}: {}",
                            error.code,
                            error.message
                        );
                        view.record_backend_error("archive session", error)
                    }
                    Ok(response) => {
                        log::warn!(
                            "[loom-ui] archive session {session_id} returned an unexpected response"
                        );
                        view.record_backend_error(
                            "archive session",
                            unexpected_response("session archive", response),
                        )
                    }
                }
            },
        );
    }

    pub(crate) fn select_session(&mut self, session: AgentSessionSnapshot, cx: &mut Context<Self>) {
        let backend = match self.backend_for_session(session.id) {
            Ok(backend) => backend,
            Err(error) => {
                self.record_backend_error("select session", error);
                cx.notify();
                return;
            }
        };
        self.github_login = None;
        self.source_dialog = None;
        self.settings_open = false;
        self.status_banner = None;
        self.review.open = false;
        self.project_child_review = None;
        let project_context = self.project_snapshot.clone().filter(|snapshot| {
            snapshot
                .agents
                .iter()
                .any(|agent| agent.session_id == session.id)
        });
        self.activate_session(session.clone());
        self.project_snapshot = project_context;
        if let Some(node_id) = self.session_node_ids.get(&session.id).cloned()
            && self.model_catalog_node_id.as_deref() != Some(node_id.as_str())
        {
            self.refresh_models_for_node_async(node_id, cx);
        }
        self.ensure_session_task_message(session.id);
        let session_id = session.id;
        let mut event_stream_epoch = self.event_stream_epoch.clone();
        let snapshot_request = backend.submit(RequestEnvelope::new(ClientRequest::Session(
            SessionRequest::GetAgentSessionInitialState { session_id },
        )));
        cx.spawn(async move |view, cx| {
            let mut snapshot = cx
                .background_spawn(async move { snapshot_request.wait().await })
                .await;
            if snapshot.result.is_err() {
                snapshot = backend
                    .submit(RequestEnvelope::new(ClientRequest::Session(
                        SessionRequest::GetAgentSessionSnapshot { session_id },
                    )))
                    .wait()
                    .await;
            }
            let cursor = match &snapshot.result {
                Ok(ServerResponse::Session(SessionResponse::AgentSessionInitialState(initial))) => {
                    Some(initial.cursor)
                }
                _ => None,
            };
            if let Ok(ServerResponse::Session(SessionResponse::AgentSessionInitialState(initial))) =
                snapshot.result.clone()
            {
                snapshot.result = Ok(ServerResponse::Session(
                    SessionResponse::AgentSessionSnapshot(initial.projection),
                ));
            }
            let events_request = backend.submit(RequestEnvelope::new(ClientRequest::Events(
                EventsRequest::GetSessionEvents {
                    session_id: Some(session_id),
                    workspace_id: None,
                    after_sequence: cursor,
                    stream_epoch: event_stream_epoch.clone(),
                },
            )));
            let mut events = cx
                .background_spawn(async move { events_request.wait().await })
                .await;
            if let Ok(ServerResponse::Events(EventsResponse::SessionEventsSnapshot {
                stream_epoch: Some(epoch),
                ..
            })) = &events.result
            {
                event_stream_epoch = Some(epoch.clone());
            }
            if matches!(
                &events.result,
                Ok(ServerResponse::Events(
                    EventsResponse::SessionEventsSnapshot { .. }
                ))
            ) {
                let refresh_request = backend.submit(RequestEnvelope::new(ClientRequest::Session(
                    SessionRequest::GetAgentSessionInitialState { session_id },
                )));
                let mut refreshed = cx
                    .background_spawn(async move { refresh_request.wait().await })
                    .await;
                if let Ok(ServerResponse::Session(SessionResponse::AgentSessionInitialState(
                    initial,
                ))) = refreshed.result.clone()
                {
                    let refreshed_cursor = initial.cursor;
                    refreshed.result = Ok(ServerResponse::Session(
                        SessionResponse::AgentSessionSnapshot(initial.projection),
                    ));
                    let retry_request = backend.submit(RequestEnvelope::new(
                        ClientRequest::Events(EventsRequest::GetSessionEvents {
                            session_id: Some(session_id),
                            workspace_id: None,
                            after_sequence: Some(refreshed_cursor),
                            stream_epoch: event_stream_epoch.clone(),
                        }),
                    ));
                    snapshot = refreshed;
                    events = cx
                        .background_spawn(async move { retry_request.wait().await })
                        .await;
                }
            }
            view.update(cx, |view, cx| {
                view.finish_async_session_load(session_id, snapshot, events, cx);
            })
            .ok();
        })
        .detach();
        cx.notify();
    }

    pub(crate) fn finish_async_session_load(
        &mut self,
        session_id: AgentSessionId,
        snapshot_response: ResponseEnvelope,
        events_response: ResponseEnvelope,
        cx: &mut Context<Self>,
    ) {
        if self.active_session.id != session_id {
            return;
        }
        let needs_transcript_page = match &snapshot_response.result {
            Ok(ServerResponse::Session(SessionResponse::AgentSessionSnapshot(projection))) => {
                projection
                    .active_run
                    .as_ref()
                    .is_some_and(|run| run.messages.is_empty())
            }
            _ => false,
        };
        let fallback_projection = match snapshot_response.result {
            Ok(ServerResponse::Session(SessionResponse::AgentSessionSnapshot(projection))) => {
                self.active_session = projection.session.clone();
                self.auto_approve_actions = projection.auto_approve_actions;
                self.session_auto_approve_actions
                    .insert(session_id, projection.auto_approve_actions);
                if let Some(run) = &projection.active_run {
                    self.session_task_cache
                        .insert(session_id, run.run.task.clone());
                    self.model = run.run.model.clone();
                }
                Some(projection)
            }
            Err(error) => {
                self.record_backend_error("load session snapshot", error);
                self.reset_projection();
                None
            }
            Ok(response) => {
                self.record_backend_error(
                    "load session snapshot",
                    unexpected_response("session snapshot", response),
                );
                self.reset_projection();
                None
            }
        };
        self.reset_projection();
        self.after_sequence = fallback_projection
            .as_ref()
            .map(|projection| projection.latest_sequence);
        match events_response.result {
            Ok(ServerResponse::Events(EventsResponse::SessionEvents {
                events,
                stream_epoch,
            })) => {
                self.event_stream_epoch = stream_epoch;
                for event in events {
                    self.after_sequence = Some(event.sequence);
                    self.consume_event(&event.event);
                }
                if self.timeline.is_empty()
                    && let Some(projection) = fallback_projection
                        .as_ref()
                        .and_then(|projection| projection.active_run.clone())
                {
                    self.apply_run_projection(projection);
                }
            }
            Ok(ServerResponse::Events(EventsResponse::SessionEventsSnapshot {
                session,
                events,
                latest_sequence,
                stream_epoch,
                ..
            })) => {
                self.event_stream_epoch = stream_epoch;
                self.active_session = session;
                self.reset_projection();
                self.after_sequence = Some(latest_sequence);
                for event in events {
                    self.after_sequence = Some(event.sequence);
                    self.consume_event(&event.event);
                }
                if self.timeline.is_empty()
                    && let Some(projection) = fallback_projection
                        .as_ref()
                        .and_then(|projection| projection.active_run.clone())
                {
                    self.apply_run_projection(projection);
                }
            }
            Err(error) => {
                if let Some(projection) = fallback_projection
                    .as_ref()
                    .and_then(|projection| projection.active_run.clone())
                {
                    self.apply_run_projection(projection);
                }
                self.record_backend_error("load session events", error);
            }
            Ok(response) => self.record_backend_error(
                "load session events",
                unexpected_response("session event stream", response),
            ),
        }
        self.ensure_session_task_message(session_id);
        if needs_transcript_page && self.active_run_id.is_some() {
            self.begin_transcript_page(None, cx);
        }
        self.refresh_review(cx);
        self.refresh_active_project_snapshot(cx);
        cx.notify();
    }

    pub(crate) fn new_session(&mut self, _: &ClickEvent, _: &mut Window, cx: &mut Context<Self>) {
        self.begin_source_dialog(SessionSourceDialogPurpose::StartSession, cx);
    }

    pub(crate) fn create_session_on_node_with_source(
        &mut self,
        node_id: String,
        name: String,
        source: Option<SessionCreationSource>,
        cx: &mut Context<Self>,
    ) {
        let creation_status = match source.as_ref() {
            Some(SessionCreationSource::GitHub(repository)) => {
                format!("Creating project and cloning {}…", repository.full_name)
            }
            Some(SessionCreationSource::LocalDirectory(_)) => {
                "Creating project and attaching directory…".to_owned()
            }
            None => "Creating project…".to_owned(),
        };
        self.record_status(creation_status);
        let Some(backend) = self.node_backends.get(&node_id).cloned() else {
            self.record_backend_error(
                "create session",
                LoomError::new(
                    ErrorCode::NotFound,
                    format!("worker node {node_id} is not connected"),
                    false,
                ),
            );
            cx.notify();
            return;
        };
        let workspace_id = self.workspace_id;
        let Some(workspace) = self
            .workspaces
            .iter()
            .find(|workspace| workspace.id == workspace_id)
            .cloned()
        else {
            self.record_backend_error(
                "create session",
                LoomError::not_found("workspace", workspace_id),
            );
            cx.notify();
            return;
        };
        let model = self.default_model.clone();
        let node_name = self
            .node_names
            .get(&node_id)
            .cloned()
            .unwrap_or_else(|| node_id.clone());
        cx.spawn(async move |view, cx| {
            let catalog = match list_models_from_backend(&backend).await {
                Ok(catalog) => catalog,
                Err(error) => {
                    view.update(cx, |view, cx| {
                        view.record_backend_error("check worker models", error);
                        cx.notify();
                    })
                    .ok();
                    return;
                }
            };
            let models = catalog.models;
            view.update(cx, |view, cx| {
                view.record_model_discovery_errors(catalog.discovery_errors);
                view.node_model_catalogs
                    .insert(node_id.clone(), models.clone());
                view.default_models = models.clone();
                cx.notify();
            })
            .ok();
            let node_models = BTreeMap::from([(node_id.clone(), models.clone())]);
            if let Err(reason) = validate_model_for_node(&node_models, &node_id, &model) {
                view.update(cx, |view, cx| {
                    view.record_backend_error(
                        "create session",
                        LoomError::invalid_state(format!(
                            "Cannot create a session on {node_name}: {reason}. Choose a model configured on this worker in Settings, then try again."
                        )),
                    );
                    cx.notify();
                })
                .ok();
                return;
            }
            let result = async {
                log::info!("registering workspace before session creation");
                let registered = backend
                    .submit(RequestEnvelope::new(ClientRequest::Workspace(WorkspaceRequest::RegisterWorkspace{
                        workspace: workspace.clone(),
                    })))
                    .wait()
                    .await;
                match registered.result? {
                    ServerResponse::Workspace(WorkspaceResponse::WorkspaceCreated(_)) => {}
                    response => {
                        return Err(unexpected_response("workspace registration", response));
                    }
                }
                let response = backend
                    .submit(RequestEnvelope::new(
                        ClientRequest::Workspace(WorkspaceRequest::CreateAgentSessionInWorkspace{ workspace_id, name }),
                    ))
                    .wait()
                    .await;
                let snapshot = match response.result? {
                    ServerResponse::Session(SessionResponse::AgentSessionCreated(snapshot)) => snapshot,
                    response => return Err(unexpected_response("session creation", response)),
                };
                log::info!("created session {}; attaching source", snapshot.id);
                let setup = match source {
                    None => Ok(()),
                    Some(SessionCreationSource::LocalDirectory(source)) => {
                        let path = source_mount_path("sources", &source, &[]);
                        let response = backend
                            .submit(RequestEnvelope::new(ClientRequest::Filesystem(FilesystemRequest::AttachSessionDirectory{
                                session_id: snapshot.id,
                                source,
                                path,
                            })))
                            .wait()
                            .await;
                        match response.result? {
                            ServerResponse::Filesystem(FilesystemResponse::SessionDirectoryAttached{ .. }) => Ok(()),
                            response => Err(unexpected_response("directory attachment", response)),
                        }
                    }
                    Some(SessionCreationSource::GitHub(repository)) => {
                        log::info!("cloning GitHub repository {} into session {}", repository.full_name, snapshot.id);
                        let path = source_mount_path("repositories", &repository.full_name, &[]);
                        let response = backend
                            .submit(RequestEnvelope::new(ClientRequest::Repository(RepositoryRequest::AttachSessionRepository{
                                session_id: snapshot.id,
                                source: repository.clone_url,
                                path,
                                revision: None,
                            })))
                            .wait()
                            .await;
                        match response.result? {
                            ServerResponse::Repository(RepositoryResponse::SessionRepositoryAttached(_)) => Ok(()),
                            response => Err(unexpected_response("repository attachment", response)),
                        }
                    }
                };
                if let Err(error) = setup {
                    log::error!("session source setup failed: {}", error.message);
                    let _ = backend
                        .submit(RequestEnvelope::new(ClientRequest::Session(SessionRequest::ArchiveAgentSession{
                            session_id: snapshot.id,
                        })))
                        .wait()
                        .await;
                    return Err(error);
                }
                Ok(snapshot)
            }
            .await;
            view.update(cx, |view, cx| match result {
                Ok(snapshot) => {
                    view.record_status("Project created successfully");
                    view.node_model_catalogs
                        .insert(node_id.clone(), models);
                    view.session_models.insert(snapshot.id, model);
                    view.session_node_ids
                        .insert(snapshot.id, node_id.clone());
                    view.sessions.push(snapshot.clone());
                    view.select_session(snapshot, cx);
                }
                Err(error) => {
                    log::error!("session creation failed: {}", error.message);
                    view.record_backend_error("create session", error)
                },
            })
            .ok();
        })
        .detach();
    }

    pub(crate) fn begin_session_rename(
        &mut self,
        session: AgentSessionSnapshot,
        is_project: bool,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.select_session(session, cx);
        self.rename_dialog = Some(RenameDialogState {
            session: self.active_session.clone(),
            input: self.active_session.name.clone(),
            is_project,
        });
        self.rename_input_state = None;
    }

    pub(crate) fn select_session_repository(
        &mut self,
        repository_id: RepositoryId,
        cx: &mut Context<Self>,
    ) {
        let session_id = self.active_session.id;
        self.selected_repository_id = Some(repository_id);
        self.review.selected_path = None;
        self.review.selected_diff = None;
        self.review.selected_file = None;
        self.review.rows.clear();
        self.review.hunk_rows.clear();
        self.review.list_state.reset(0);
        self.review.selection_revision += 1;
        self.review.vcs = None;
        self.dispatch(
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
                    Ok(ServerResponse::Repository(RepositoryResponse::VcsStatus(status))) => {
                        view.review.vcs = Some(status)
                    }
                    Err(error) => {
                        view.review.vcs = None;
                        view.record_status(format!("VCS review unavailable: {error}"));
                    }
                    Ok(response) => view.record_backend_error(
                        "VCS review refresh",
                        unexpected_response("VCS status", response),
                    ),
                }
            },
        );
        cx.notify();
    }

    pub(crate) fn detach_session_repository(
        &mut self,
        repository_id: RepositoryId,
        cx: &mut Context<Self>,
    ) {
        self.dispatch(
            cx,
            ClientRequest::Repository(RepositoryRequest::DetachSessionRepository {
                session_id: self.active_session.id,
                repository_id,
            }),
            move |view, response, cx| match response.result {
                Ok(ServerResponse::Repository(RepositoryResponse::SessionRepositoryDetached)) => {
                    view.session_repositories
                        .retain(|repository| repository.id != repository_id);
                    if view.selected_repository_id == Some(repository_id) {
                        view.selected_repository_id = view
                            .session_repositories
                            .first()
                            .map(|repository| repository.id);
                    }
                    view.refresh_review(cx);
                }
                Err(error) => view.record_backend_error("detach repository", error),
                Ok(response) => view.record_backend_error(
                    "detach repository",
                    unexpected_response("repository detachment", response),
                ),
            },
        );
    }

    pub(crate) fn detach_session_directory(&mut self, path: String, cx: &mut Context<Self>) {
        self.dispatch(
            cx,
            ClientRequest::Filesystem(FilesystemRequest::DetachSessionDirectory {
                session_id: self.active_session.id,
                path: path.clone(),
            }),
            move |view, response, cx| match response.result {
                Ok(ServerResponse::Filesystem(FilesystemResponse::SessionDirectoryDetached)) => {
                    view.session_directories
                        .retain(|directory| directory.path != path);
                    view.session_repositories.retain(|repository| {
                        repository.path != path && !repository.path.starts_with(&format!("{path}/"))
                    });
                    if !view
                        .session_repositories
                        .iter()
                        .any(|repository| Some(repository.id) == view.selected_repository_id)
                    {
                        view.selected_repository_id = view
                            .session_repositories
                            .first()
                            .map(|repository| repository.id);
                    }
                    view.refresh_review(cx);
                }
                Err(error) => view.record_backend_error("detach directory", error),
                Ok(response) => view.record_backend_error(
                    "detach directory",
                    unexpected_response("directory detachment", response),
                ),
            },
        );
    }
}
