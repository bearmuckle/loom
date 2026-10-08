use super::*;

impl LoomView {
    pub(crate) fn schedule_project_poll(&mut self, cx: &mut Context<Self>) {
        if self.project_poll_scheduled
            || !self.project_root_is_active()
            || !self.project_has_live_children()
        {
            return;
        }
        self.project_poll_scheduled = true;
        cx.spawn(async move |view, cx| {
            #[cfg(target_family = "wasm")]
            {
                let promise = js_sys::Promise::new(&mut |resolve, _reject| {
                    if let Some(window) = web_sys::window() {
                        let _ = window
                            .set_timeout_with_callback_and_timeout_and_arguments_0(&resolve, 1000);
                    }
                });
                let _ = wasm_bindgen_futures::JsFuture::from(promise).await;
            }
            #[cfg(not(target_family = "wasm"))]
            cx.background_spawn(async {
                std::thread::sleep(Duration::from_secs(1));
            })
            .await;
            view.update(cx, |view, cx| {
                if view.project_root_is_active() {
                    view.poll_project_once(cx);
                } else {
                    view.project_poll_scheduled = false;
                }
            })
            .ok();
        })
        .detach();
    }

    pub(crate) fn poll_project_once(&mut self, cx: &mut Context<Self>) {
        let Some(project) = self.project_snapshot.as_ref() else {
            self.project_poll_scheduled = false;
            return;
        };
        let project_id = project.project_id;
        let root_session_id = project.root_session_id;
        let workspace_id = self.active_session.workspace_id;
        let member_ids = project
            .agents
            .iter()
            .map(|agent| agent.session_id)
            .collect::<BTreeSet<_>>();
        let after_sequence = self.project_feed_after_sequence;
        let stream_epoch = self.project_feed_epoch.clone();
        self.dispatch(
            cx,
            ClientRequest::Events(EventsRequest::GetSessionEvents {
                session_id: None,
                workspace_id: Some(workspace_id),
                after_sequence,
                stream_epoch,
            }),
            move |view, response, cx| {
                if view.active_session.id != root_session_id
                    || view.active_session.workspace_id != workspace_id
                {
                    view.project_poll_scheduled = false;
                    return;
                }
                view.project_poll_scheduled = false;
                match response.result {
                    Ok(ServerResponse::Events(EventsResponse::WorkspaceEvents {
                        workspace_id: response_workspace,
                        events,
                        stream_epoch,
                    })) if response_workspace == workspace_id => {
                        view.project_feed_epoch = stream_epoch;
                        if let Some(latest) = events.iter().map(workspace_feed_event_sequence).max()
                        {
                            view.project_feed_after_sequence = Some(
                                view.project_feed_after_sequence
                                    .map_or(latest, |current| current.max(latest)),
                            );
                        }
                        if events
                            .iter()
                            .any(|event| is_project_workspace_event(event, project_id, &member_ids))
                        {
                            view.project_snapshot_stale = true;
                        }
                    }
                    Ok(ServerResponse::Events(EventsResponse::WorkspaceEventsSnapshot {
                        workspace_id: response_workspace,
                        events,
                        latest_sequence,
                        stream_epoch,
                        ..
                    })) if response_workspace == workspace_id => {
                        view.project_feed_epoch = stream_epoch;
                        view.project_feed_after_sequence = Some(latest_sequence);
                        view.project_snapshot_stale = true;
                        if events
                            .iter()
                            .any(|event| is_project_workspace_event(event, project_id, &member_ids))
                        {
                            view.project_snapshot_stale = true;
                        }
                    }
                    Err(error) => view.record_backend_error("project event stream", error),
                    Ok(response) => view.record_backend_error(
                        "project event stream",
                        unexpected_response("project event stream", response),
                    ),
                }
                if view.project_snapshot_stale {
                    view.refresh_active_project_snapshot(cx);
                }
                // A manager that ended its turn while children still run is
                // woken by the server. Keep consuming the root session's events
                // alongside the project feed so that wake becomes visible.
                if view.project_root_is_active()
                    && view.project_has_live_children()
                    && !view.run_is_active()
                {
                    view.poll_run_once(cx);
                }
                view.schedule_project_poll(cx);
            },
        );
    }

    /// Applies newly journaled session events through the connection worker.
    pub(crate) fn poll_run_once(&mut self, cx: &mut Context<Self>) {
        self.dispatch(
            cx,
            ClientRequest::Events(EventsRequest::GetSessionEvents {
                session_id: Some(self.active_session.id),
                workspace_id: None,
                after_sequence: self.after_sequence,
                stream_epoch: self.event_stream_epoch.clone(),
            }),
            |view, response, cx| {
                match response.result {
                    Ok(ServerResponse::Events(EventsResponse::SessionEvents {
                        events,
                        stream_epoch,
                    })) => {
                        view.event_stream_epoch = stream_epoch;
                        for event in events {
                            view.after_sequence = Some(event.sequence);
                            view.consume_event(&event.event);
                        }
                    }
                    Ok(ServerResponse::Events(EventsResponse::SessionEventsSnapshot {
                        session,
                        events,
                        latest_sequence,
                        stream_epoch,
                        ..
                    })) => {
                        view.event_stream_epoch = stream_epoch;
                        view.active_session = session;
                        view.reset_projection();
                        view.after_sequence = Some(latest_sequence);
                        for event in events {
                            view.after_sequence = Some(event.sequence);
                            view.consume_event(&event.event);
                        }
                    }
                    Err(error) => view.record_backend_error("session event stream", error),
                    Ok(response) => view.record_backend_error(
                        "session event stream",
                        unexpected_response("session event stream", response),
                    ),
                }
                if view.project_snapshot_stale {
                    view.refresh_active_project_snapshot(cx);
                }
                view.run_poll_scheduled = false;
                view.schedule_run_poll(cx);
            },
        );
    }

    pub(crate) fn run_is_active(&self) -> bool {
        matches!(
            self.run_state,
            Some(AgentRunState::Planning | AgentRunState::Executing | AgentRunState::Evaluating)
        )
    }

    /// Polls the journal while a run is active. The protocol keeps the event
    /// stream resumable, so polling is sufficient for both local and remote
    /// connections without making the UI depend on a transport-specific push
    /// implementation.
    /// Whether the session event stream should keep being polled. A run that is
    /// parked on a durable project join reports `Paused`, but the server
    /// resumes it when its children finish, so polling must continue past the
    /// active states or the wait never appears to complete.
    pub(crate) fn run_should_poll(&self) -> bool {
        self.run_is_active() || matches!(self.run_state, Some(AgentRunState::Paused))
    }

    pub(crate) fn schedule_run_poll(&mut self, cx: &mut Context<Self>) {
        if self.run_poll_scheduled || !self.run_should_poll() {
            return;
        }
        self.run_poll_scheduled = true;
        let active = self.run_is_active();
        cx.spawn(async move |view, cx| {
            let delay = if active { 250 } else { 1_000 };
            #[cfg(target_family = "wasm")]
            {
                let promise = js_sys::Promise::new(&mut |resolve, _reject| {
                    if let Some(window) = web_sys::window() {
                        let _ = window.set_timeout_with_callback_and_timeout_and_arguments_0(
                            &resolve,
                            delay as i32,
                        );
                    }
                });
                let _ = wasm_bindgen_futures::JsFuture::from(promise).await;
            }
            #[cfg(not(target_family = "wasm"))]
            cx.background_spawn(async move {
                std::thread::sleep(Duration::from_millis(delay));
            })
            .await;
            view.update(cx, |view, cx| {
                if !view.run_should_poll() {
                    view.run_poll_scheduled = false;
                } else if view.transcript_loading {
                    // A transcript page rebuild replaces the message portion of
                    // the timeline. Defer draining the event stream until the
                    // page has been applied, or events consumed in the meantime
                    // are discarded and the view looks sparse.
                    view.run_poll_scheduled = false;
                    view.schedule_run_poll(cx);
                } else {
                    view.poll_run_once(cx);
                }
            })
            .ok();
        })
        .detach();
    }

    pub(crate) fn start_run_polling(&mut self, cx: &mut Context<Self>) {
        if self.run_poll_scheduled || self.active_run_id.is_none() {
            return;
        }
        self.run_poll_scheduled = true;
        self.poll_run_once(cx);
    }

    pub(crate) fn schedule_worker_node_poll(&mut self, cx: &mut Context<Self>) {
        let node_ids = self
            .worker_nodes
            .iter()
            .filter(|node| {
                node.connection.is_some() && !self.worker_node_polls_scheduled.contains(&node.id)
            })
            .map(|node| node.id)
            .collect::<Vec<_>>();
        for id in node_ids {
            self.worker_node_polls_scheduled.insert(id);
            cx.spawn(async move |view, cx| {
                #[cfg(target_family = "wasm")]
                {
                    let promise = js_sys::Promise::new(&mut |resolve, _reject| {
                        if let Some(window) = web_sys::window() {
                            // Poll promptly in the browser so a dropped socket
                            // is noticed while the client is otherwise idle.
                            let _ = window.set_timeout_with_callback_and_timeout_and_arguments_0(
                                &resolve, 3_000,
                            );
                        }
                    });
                    let _ = wasm_bindgen_futures::JsFuture::from(promise).await;
                }
                #[cfg(not(target_family = "wasm"))]
                cx.background_spawn(async {
                    std::thread::sleep(Duration::from_secs(10));
                })
                .await;
                view.update(cx, |view, cx| view.poll_worker_node_once(id, cx))
                    .ok();
            })
            .detach();
        }
    }

    #[cfg(not(target_family = "wasm"))]
    pub(crate) fn reconnect_configured_worker_nodes(&mut self, cx: &mut Context<Self>) {
        let candidates = self
            .worker_nodes
            .iter()
            .filter(|node| {
                !node.is_local
                    && node.connection.is_none()
                    && node.connection_state != WorkerConnectionState::Connecting
            })
            .filter_map(|node| node.url.as_ref().map(|url| (node.id, url.clone())))
            .collect::<Vec<_>>();
        if candidates.is_empty() {
            return;
        }
        let workspace_id = self.workspace_id;
        for (id, url) in candidates {
            if worker_url_embeds_credential(&url) {
                self.set_worker_node_connection_failure(
                    id,
                    &url,
                    "This saved URL contains credentials. Remove them from the URL and reconnect with the token in the separate access-token field.".to_owned(),
                );
                continue;
            }
            if let Some(node) = self.worker_nodes.iter_mut().find(|node| node.id == id) {
                node.connection_state = WorkerConnectionState::Connecting;
                node.connection_detail = None;
            }
            let candidate_url = url.clone();
            cx.spawn(async move |view, cx| {
                let result = cx
                    .background_spawn(async move {
                        let credentials = PeerCredentialStore::new();
                        let token = match credentials.get(workspace_id, &candidate_url) {
                            Ok(Some(token)) => token,
                            Ok(None) => {
                                return Err((
                                    WorkerConnectionStage::CredentialRead,
                                    LoomError::new(
                                        ErrorCode::AuthenticationRequired,
                                        "no saved worker credential was found",
                                        false,
                                    ),
                                    false,
                                ));
                            }
                            Err(error) => {
                                return Err((WorkerConnectionStage::CredentialRead, error, false));
                            }
                        };
                        let connection = ClientConnection::remote(candidate_url.clone(), token)
                            .map_err(|error| (WorkerConnectionStage::Transport, error, false))?;
                        if let Err(error) = negotiate(&connection).map(|_| ()) {
                            let cleanup_failed = connection.close().is_err();
                            return Err((
                                WorkerConnectionStage::Negotiation,
                                error,
                                cleanup_failed,
                            ));
                        }
                        let status = match worker_node_status(&connection) {
                            Ok(status) => status,
                            Err(error) => {
                                let cleanup_failed = connection.close().is_err();
                                return Err((WorkerConnectionStage::Status, error, cleanup_failed));
                            }
                        };
                        Ok::<_, (WorkerConnectionStage, LoomError, bool)>((
                            connection,
                            status,
                            candidate_url,
                        ))
                    })
                    .await;
                view.update(cx, |view, cx| {
                    let is_pending = view.worker_nodes.iter().any(|node| {
                        node.id == id
                            && !node.is_local
                            && node.url.as_deref() == Some(url.as_str())
                            && node.connection.is_none()
                    });
                    match result {
                        Ok((connection, status, connected_url)) if is_pending => {
                            view.attach_reconnected_worker_node(
                                id,
                                &connected_url,
                                connection,
                                status,
                                cx,
                            );
                        }
                        Ok((connection, _, _)) => {
                            cx.spawn(async move |view, cx| {
                                let result =
                                    cx.background_spawn(async move { connection.close() })                                    .await;
                                        if result.is_err() {
                                            view.update(cx, |view, cx| {
                                                view.record_status(
                                                    "A stale worker connection could not be closed cleanly.",
                                                );
                                                cx.notify();
                                            })
                                            .ok();
                                        }
                            })
                            .detach();
                        }
                        Err((stage, error, cleanup_failed)) if is_pending => {
                            view.fail_worker_node_connection(
                                id,
                                &url,
                                stage,
                                &error,
                                None,
                                cleanup_failed,
                            );
                        }
                        Err(_) => {}
                    }
                    cx.notify();
                })
                .ok();
            })
            .detach();
        }
        cx.notify();
    }

    pub(crate) fn poll_worker_node_once(&mut self, id: u64, cx: &mut Context<Self>) {
        let Some(connection) = self
            .worker_nodes
            .iter()
            .find(|node| node.id == id)
            .and_then(|node| node.connection.clone())
        else {
            self.worker_node_polls_scheduled.remove(&id);
            return;
        };

        cx.spawn(async move |view, cx| {
            #[cfg(not(target_family = "wasm"))]
            let results = cx
                .background_spawn(async move { worker_node_status(&connection) })
                .await;
            #[cfg(target_family = "wasm")]
            let results = worker_node_status_async(&connection).await;

            view.update(cx, |view, cx| {
                let mut recovered = false;
                if let Some(node_index) = view.worker_nodes.iter().position(|node| node.id == id) {
                    let status_message = match results {
                        Ok(status) => {
                            recovered =
                                !view.worker_nodes[node_index].status.online && status.online;
                            view.worker_nodes[node_index].severe_load_streak =
                                next_severe_load_streak(
                                    view.worker_nodes[node_index].severe_load_streak,
                                    &status.resources,
                                );
                            update_worker_node_status(&mut view.worker_nodes, id, status)
                        }
                        Err(_) => {
                            let node = &mut view.worker_nodes[node_index];
                            node.severe_load_streak = 0;
                            if node.status.online {
                                node.status.online = false;
                                Some(format!("Worker node {} is unavailable", node.status.name))
                            } else {
                                None
                            }
                        }
                    };
                    if let Some(message) = status_message {
                        view.record_status(message);
                    }
                    let node = &view.worker_nodes[node_index];
                    view.node_names
                        .insert(node.status.node_id.clone(), worker_node_display_name(node));
                }
                view.worker_node_polls_scheduled.remove(&id);
                #[cfg(target_family = "wasm")]
                {
                    view.detect_browser_connection_loss(cx);
                    if !view.connected {
                        return;
                    }
                }
                view.schedule_worker_node_poll(cx);
                if recovered {
                    view.reload_sessions(cx);
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }
}
