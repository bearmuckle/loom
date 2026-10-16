use super::*;

impl LoomView {
    pub(crate) fn activate_session(&mut self, session: AgentSessionSnapshot) {
        let workspace_changed = self.active_session.workspace_id != session.workspace_id;
        self.active_session = session;
        self.project_snapshot = None;
        self.project_snapshot_stale = false;
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
        self.ask_before_destructive_actions = self
            .session_ask_before_destructive_actions
            .get(&self.active_session.id)
            .copied()
            .unwrap_or(false);
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
        self.review.clear_diff();
        self.review.selected_file = None;
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
                    SessionResponse::AgentSessionSnapshot(Box::new(initial.projection)),
                ))
            }
            result => result,
        };
        let mut needs_transcript_page = false;
        let fallback_projection = match snapshot_result {
            Ok(ServerResponse::Session(SessionResponse::AgentSessionSnapshot(projection))) => {
                // Any projection that carries a run is paged, whether or not it
                // already holds messages: a snapshot carries the messages its
                // live run handle knows, which is not necessarily the run's
                // whole transcript.
                needs_transcript_page = projection.active_run.is_some();
                // The fallback snapshot carries the durable cursor, so the
                // events request below stays bounded to what was journaled after
                // it instead of asking for the whole retained journal.
                if event_cursor.is_none() {
                    event_cursor = Some(projection.latest_sequence);
                }
                self.active_session = projection.session.clone();
                self.session_state = self.active_session.state;
                self.auto_approve_actions = projection.auto_approve_actions;
                self.session_auto_approve_actions
                    .insert(self.active_session.id, projection.auto_approve_actions);
                self.ask_before_destructive_actions =
                    destructive_actions_require_approval(projection.approval_policy.destructive);
                self.session_ask_before_destructive_actions
                    .insert(self.active_session.id, self.ask_before_destructive_actions);
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
            fallback_projection
                .as_ref()
                .map(|projection| projection.latest_sequence),
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
        // The dialog steps aside while the rename is in flight, which is what a
        // user saw before it was the covering surface. A refusal brings it
        // back; a success leaves the conversation uncovered.
        if self.covering_surface() == Some(CoveringSurface::Rename) {
            self.close_covering_surface();
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
                    view.show_surface(CoveringSurface::Rename);
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
                        let was_active = view.forget_archived_session(snapshot.id);
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

    /// Forgets an archived session and, when a whole project was archived, every
    /// descendant the backend cascaded. Returns whether the active session was
    /// among the removed sessions.
    pub(crate) fn forget_archived_session(&mut self, archived_id: AgentSessionId) -> bool {
        let mut removed = BTreeSet::new();
        removed.insert(archived_id);
        for project in self
            .project_tree_snapshots
            .iter()
            .chain(self.project_snapshot.iter())
            .chain(self.archived_sessions.projects.iter())
        {
            if project.root_session_id == archived_id {
                for agent in &project.agents {
                    removed.insert(agent.session_id);
                }
            }
        }
        self.sessions
            .retain(|session| !removed.contains(&session.id));
        for session_id in &removed {
            self.session_node_ids.remove(session_id);
            self.session_task_cache.remove(session_id);
            self.session_models.remove(session_id);
            self.session_auto_approve_actions.remove(session_id);
            self.session_ask_before_destructive_actions
                .remove(session_id);
        }
        // Drop the archived sessions from every cached project snapshot so the
        // tree view stops rendering them without waiting for a refresh.
        let prune = |project: &mut loom_core::ProjectSnapshot| {
            project
                .agents
                .retain(|agent| !removed.contains(&agent.session_id));
            project
                .tasks
                .retain(|task| !removed.contains(&task.target_session_id));
            project
                .worktrees
                .retain(|worktree| !removed.contains(&worktree.child_session_id));
        };
        self.project_tree_snapshots.retain_mut(|project| {
            if removed.contains(&project.root_session_id) {
                return false;
            }
            prune(project);
            true
        });
        // The archived-session surface keeps its own copy of the entries and the
        // project snapshots it groups them by, so a cascade that removed whole
        // projects cannot leave stale rows behind there.
        self.archived_sessions
            .entries
            .retain(|entry| !removed.contains(&entry.session.id));
        self.archived_sessions
            .force
            .retain(|session_id| !removed.contains(session_id));
        self.archived_sessions.projects.retain_mut(|project| {
            if removed.contains(&project.root_session_id) {
                return false;
            }
            prune(project);
            true
        });
        let project_root_removed = self
            .project_snapshot
            .as_ref()
            .is_some_and(|project| removed.contains(&project.root_session_id));
        if project_root_removed {
            self.project_snapshot = None;
            self.project_snapshot_stale = false;
            self.project_feed_after_sequence = None;
            self.project_poll_scheduled = false;
        } else if let Some(project) = self.project_snapshot.as_mut() {
            prune(project);
        }
        removed.contains(&self.active_session.id)
    }

    /// Dismisses every surface that covers the conversation.
    ///
    /// Navigating to a project always returns to the conversation, including
    /// when the project is already the active one: otherwise a click on the
    /// open project looks like it did nothing while a covering surface is in
    /// front of it and the user has to close it by hand first. The one value
    /// that names the covering surface means no list of them has to be kept
    /// here.
    pub(crate) fn dismiss_surfaces(&mut self) {
        self.close_covering_surface();
        self.session_drawer_open = false;
    }

    pub(crate) fn select_session(&mut self, session: AgentSessionSnapshot, cx: &mut Context<Self>) {
        if let Err(error) = self.backend_for_session(session.id) {
            self.record_backend_error("select session", error);
            cx.notify();
            return;
        }
        self.dismiss_surfaces();
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
        self.begin_session_data_load(session.id, cx);
        cx.notify();
    }

    /// Requests the session's durable projection plus the events after its
    /// cursor and applies both through [`Self::finish_async_session_load`].
    ///
    /// Interactive selection and poll-driven resync share this ladder, so a
    /// resync rebuilds the conversation from durable state (the run projection
    /// and the newest transcript page) instead of replaying whatever partial
    /// event window the backend returned.
    pub(crate) fn begin_session_data_load(
        &mut self,
        session_id: AgentSessionId,
        cx: &mut Context<Self>,
    ) {
        let backend = match self.backend_for_session(session_id) {
            Ok(backend) => backend,
            Err(error) => {
                self.record_backend_error("load session", error);
                cx.notify();
                return;
            }
        };
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
            let mut request_cursor = match &snapshot.result {
                Ok(ServerResponse::Session(SessionResponse::AgentSessionInitialState(initial))) => {
                    Some(initial.cursor)
                }
                // The snapshot fallback carries the durable cursor in
                // `latest_sequence`, so the events request stays bounded to
                // what was journaled after the projection was read.
                Ok(ServerResponse::Session(SessionResponse::AgentSessionSnapshot(projection))) => {
                    Some(projection.latest_sequence)
                }
                _ => None,
            };
            if let Ok(ServerResponse::Session(SessionResponse::AgentSessionInitialState(initial))) =
                snapshot.result.clone()
            {
                snapshot.result = Ok(ServerResponse::Session(
                    SessionResponse::AgentSessionSnapshot(Box::new(initial.projection)),
                ));
            }
            let events_request = backend.submit(RequestEnvelope::new(ClientRequest::Events(
                EventsRequest::GetSessionEvents {
                    session_id: Some(session_id),
                    workspace_id: None,
                    after_sequence: request_cursor,
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
                        SessionResponse::AgentSessionSnapshot(Box::new(initial.projection)),
                    ));
                    let retry_request = backend.submit(RequestEnvelope::new(
                        ClientRequest::Events(EventsRequest::GetSessionEvents {
                            session_id: Some(session_id),
                            workspace_id: None,
                            after_sequence: Some(refreshed_cursor),
                            stream_epoch: event_stream_epoch.clone(),
                        }),
                    ));
                    request_cursor = Some(refreshed_cursor);
                    snapshot = refreshed;
                    events = cx
                        .background_spawn(async move { retry_request.wait().await })
                        .await;
                }
            }
            view.update(cx, |view, cx| {
                view.finish_async_session_load(session_id, request_cursor, snapshot, events, cx);
            })
            .ok();
        })
        .detach();
    }

    pub(crate) fn finish_async_session_load(
        &mut self,
        session_id: AgentSessionId,
        request_cursor: Option<EventSequence>,
        snapshot_response: ResponseEnvelope,
        events_response: ResponseEnvelope,
        cx: &mut Context<Self>,
    ) {
        if self.active_session.id != session_id {
            return;
        }
        // A projection that carries a run is always paged. The old gate only
        // paged a projection *without* messages, so a snapshot that carried the
        // messages its live run handle knows was never paged: when the initial
        // event batch was non-empty, the timeline stayed exactly that batch, and
        // a window of one event showed as the whole conversation (GitHub issue
        // #261).
        let needs_transcript_page = match &snapshot_response.result {
            Ok(ServerResponse::Session(SessionResponse::AgentSessionSnapshot(projection))) => {
                projection.active_run.is_some()
            }
            _ => false,
        };
        let event_batch_len = match &events_response.result {
            Ok(ServerResponse::Events(EventsResponse::SessionEvents { events, .. }))
            | Ok(ServerResponse::Events(EventsResponse::SessionEventsSnapshot {
                events, ..
            })) => events.len(),
            _ => 0,
        };
        let timeline_before = self.timeline.len();
        let fallback_projection = match snapshot_response.result {
            Ok(ServerResponse::Session(SessionResponse::AgentSessionSnapshot(projection))) => {
                self.active_session = projection.session.clone();
                self.session_state = self.active_session.state;
                self.auto_approve_actions = projection.auto_approve_actions;
                self.session_auto_approve_actions
                    .insert(session_id, projection.auto_approve_actions);
                self.ask_before_destructive_actions =
                    destructive_actions_require_approval(projection.approval_policy.destructive);
                self.session_ask_before_destructive_actions
                    .insert(session_id, self.ask_before_destructive_actions);
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
        let fallback_latest_sequence = fallback_projection
            .as_ref()
            .map(|projection| projection.latest_sequence);
        let fallback_message_count = fallback_projection
            .as_ref()
            .and_then(|projection| projection.active_run.as_ref())
            .map_or(0, |run| run.messages.len());
        let fallback_run = fallback_projection
            .as_ref()
            .and_then(|projection| projection.active_run.clone());
        match events_response.result {
            Ok(ServerResponse::Events(EventsResponse::SessionEvents {
                events,
                stream_epoch,
            })) => {
                self.event_stream_epoch = stream_epoch;
                self.rebuild_projection_from_events(
                    events,
                    Self::rebuild_cursor_floor(request_cursor, fallback_latest_sequence),
                    fallback_run,
                );
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
                self.session_state = self.active_session.state;
                self.rebuild_projection_from_events(events, Some(latest_sequence), fallback_run);
            }
            Err(error) => {
                self.rebuild_projection_from_events(Vec::new(), None, fallback_run);
                self.record_backend_error("load session events", error);
            }
            Ok(response) => {
                // The response is unusable, so the projection stays reset
                // rather than mixing whatever is on screen with a batch the
                // backend did not send.
                self.reset_projection();
                self.record_backend_error(
                    "load session events",
                    unexpected_response("session event stream", response),
                )
            }
        }
        // The reopen ladder's outcome, so a live deployment shows which durable
        // half rebuilt the conversation and what the event window added to it.
        log::info!(
            "[loom-ui] reopen session {session_id}: {event_batch_len} events at cursor {request_cursor:?}, durable projection {fallback_latest_sequence:?} carrying {fallback_message_count} messages, transcript page needed {needs_transcript_page}, active run {:?}, timeline {timeline_before} -> {}",
            self.active_run_id,
            self.timeline.len()
        );
        self.ensure_session_task_message(session_id);
        if needs_transcript_page && self.active_run_id.is_some() {
            self.begin_transcript_page(None, cx);
        }
        self.refresh_review(cx);
        self.refresh_active_project_snapshot(cx);
        cx.notify();
    }

    /// The most projects whose source is looked up in one refresh, so opening
    /// the client does not issue a request per project at once.
    pub(crate) const SESSION_SOURCE_LOOKUPS_PER_REFRESH: usize = 6;

    /// Remembers the active project's main source from the sources its session
    /// load just refreshed, so opening a project fills its row's second line.
    pub(crate) fn remember_active_session_source(&mut self) {
        let label = self
            .session_repositories
            .first()
            .map(|repository| source_context_label(&repository.source))
            .or_else(|| {
                self.session_directories
                    .first()
                    .map(|directory| source_context_label(&directory.source))
            });
        let session_id = self.active_session.id;
        match label {
            Some(label) => {
                self.session_sources.insert(session_id, label);
            }
            None => {
                self.session_sources.remove(&session_id);
            }
        }
    }

    /// Records a source reported by a per-session lookup.
    ///
    /// A repository outranks a folder, and the label is only set once one of
    /// the two answers is known, so the responses may arrive in either order.
    pub(crate) fn record_session_source_candidate(
        &mut self,
        session_id: AgentSessionId,
        repository: Option<String>,
        directory: Option<String>,
    ) {
        let candidate = self
            .session_source_lookups
            .entry(session_id)
            .or_insert_with(|| (None, None));
        if let Some(repository) = repository {
            candidate.0 = Some(repository);
        }
        if let Some(directory) = directory {
            candidate.1 = Some(directory);
        }
        if let Some(label) = candidate.0.clone().or_else(|| candidate.1.clone()) {
            self.session_sources.insert(session_id, label);
        }
    }

    /// Asks the backend where each project without a known source came from, so
    /// a project row can show its main source without the project being opened.
    /// Every project is asked at most once, and the fan-out per refresh is
    /// bounded.
    pub(crate) fn refresh_session_source_labels(&mut self, cx: &mut Context<Self>) {
        for session_id in self.session_source_lookup_candidates() {
            self.session_source_lookups.insert(session_id, (None, None));
            self.dispatch(
                cx,
                ClientRequest::Repository(RepositoryRequest::ListSessionRepositories {
                    session_id,
                }),
                move |view, response, _| {
                    if let Ok(ServerResponse::Repository(
                        RepositoryResponse::SessionRepositories { repositories },
                    )) = response.result
                    {
                        view.record_session_source_candidate(
                            session_id,
                            repositories
                                .first()
                                .map(|repository| repository.source.clone()),
                            None,
                        );
                    }
                },
            );
            self.dispatch(
                cx,
                ClientRequest::Filesystem(FilesystemRequest::ListSessionDirectories { session_id }),
                move |view, response, _| {
                    if let Ok(ServerResponse::Filesystem(
                        FilesystemResponse::SessionDirectories { directories },
                    )) = response.result
                    {
                        view.record_session_source_candidate(
                            session_id,
                            None,
                            directories
                                .first()
                                .map(|directory| directory.source.clone()),
                        );
                    }
                },
            );
        }
    }

    /// Projects whose main source is still unknown, in list order and bounded
    /// per refresh. A delegated agent is skipped: it appears under its
    /// project's own row, which carries the project's source.
    pub(crate) fn session_source_lookup_candidates(&self) -> Vec<AgentSessionId> {
        self.sessions
            .iter()
            .filter(|session| !self.session_sources.contains_key(&session.id))
            .filter(|session| !self.session_source_lookups.contains_key(&session.id))
            .filter(|session| !self.session_is_project_child(session.id))
            .take(Self::SESSION_SOURCE_LOOKUPS_PER_REFRESH)
            .map(|session| session.id)
            .collect()
    }

    /// Whether a known project lists this session as one of its delegated
    /// agents rather than as its root.
    fn session_is_project_child(&self, session_id: AgentSessionId) -> bool {
        self.project_tree_snapshots.iter().any(|project| {
            project.root_session_id != session_id
                && project
                    .agents
                    .iter()
                    .any(|agent| agent.session_id == session_id)
        })
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
            Some(SessionCreationSource::GitHub(source)) => {
                if source.reuse_local {
                    format!(
                        "Creating project from cached {} (refreshing it on the worker before use)…",
                        source.full_name
                    )
                } else {
                    format!("Creating project and cloning {}…", source.full_name)
                }
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
        let known_workspace = if node_id == self.default_backend_node_id {
            self.workspaces
                .iter()
                .find(|workspace| workspace.id == self.workspace_id)
                .cloned()
        } else {
            self.node_workspaces
                .get(&node_id)
                .and_then(|workspaces| active_workspace(workspaces))
                .cloned()
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
                // A worker keeps its own workspace; the client never shares a
                // home workspace across the fleet. Create one on demand when the
                // worker has none yet.
                let workspace = match known_workspace {
                    Some(workspace) => workspace,
                    None => resolve_or_create_workspace(&backend).await?,
                };
                let workspace_id = workspace.id;
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
                    Some(SessionCreationSource::GitHub(source)) => {
                        log::info!(
                            "preparing GitHub repository {} for session {} (reuse_local={})",
                            source.full_name,
                            snapshot.id,
                            source.reuse_local
                        );
                        let path = source_mount_path("repositories", &source.full_name, &[]);
                        let response = backend
                            .submit(RequestEnvelope::new(ClientRequest::Repository(RepositoryRequest::AttachSessionRepository{
                                session_id: snapshot.id,
                                source: source.clone_url,
                                path,
                                revision: None,
                                reuse_local: source.reuse_local,
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
        self.show_surface(CoveringSurface::Rename);
        self.rename_dialog = Some(RenameDialogState {
            session: self.active_session.clone(),
            input: self.active_session.name.clone(),
            is_project,
        });
        self.rename_input_state = None;
    }

    /// Closes the rename dialog, which is the surface its own close control
    /// belongs to.
    pub(crate) fn close_rename_dialog(&mut self, cx: &mut Context<Self>) {
        if self.covering_surface() == Some(CoveringSurface::Rename) {
            // Closing the surface drops the dialog with its input state.
            self.close_covering_surface();
        } else {
            self.rename_dialog = None;
            self.rename_input_state = None;
        }
        cx.notify();
    }

    pub(crate) fn select_session_repository(
        &mut self,
        repository_id: RepositoryId,
        cx: &mut Context<Self>,
    ) {
        let session_id = self.active_session.id;
        self.selected_repository_id = Some(repository_id);
        self.review.selected_path = None;
        self.review.clear_diff();
        self.review.selected_file = None;
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

/// Resolves the workspace a project should be created in on a worker that has
/// none yet, creating a `Default` there. A worker with workspaces uses one of
/// its own; the client never shares a workspace across workers.
async fn resolve_or_create_workspace(
    backend: &BackendWorker,
) -> Result<WorkspaceRecord, LoomError> {
    let listing = backend
        .submit(RequestEnvelope::new(ClientRequest::Workspace(
            WorkspaceRequest::ListWorkspaces,
        )))
        .wait()
        .await;
    let workspaces = match listing.result? {
        ServerResponse::Workspace(WorkspaceResponse::Workspaces { workspaces }) => workspaces,
        response => return Err(unexpected_response("workspace list", response)),
    };
    if let Some(workspace) = active_workspace(&workspaces) {
        return Ok(workspace.clone());
    }
    let created = backend
        .submit(RequestEnvelope::new(ClientRequest::Workspace(
            WorkspaceRequest::CreateWorkspace {
                name: "Default".to_owned(),
            },
        )))
        .wait()
        .await;
    match created.result? {
        ServerResponse::Workspace(WorkspaceResponse::WorkspaceCreated(workspace)) => Ok(workspace),
        response => Err(unexpected_response("workspace creation", response)),
    }
}
