use super::*;
#[cfg(not(target_family = "wasm"))]
use crate::connection::resume_github_identity;

/// How often the open session's event stream is polled while this client's run
/// is active: the transcript streams at this cadence.
const ACTIVE_RUN_POLL_MILLIS: u64 = 250;

/// How often the stream is polled while the run is parked on a durable project
/// join. The server resumes such a run when its children finish, so the wait
/// still has to become visible, and a second is enough for that.
const PAUSED_RUN_POLL_MILLIS: u64 = 1_000;

/// How often the stream is polled while the session is idle.
///
/// A worker this client reached at an address serves other people too, and
/// their runs are journaled whether or not this client started them. The
/// cadence only bounds how long another person's run takes to appear here: the
/// first event it applies makes that run active on this client as well, and the
/// poll then follows [`ACTIVE_RUN_POLL_MILLIS`].
const IDLE_SESSION_POLL_MILLIS: u64 = 2_000;

/// How often the workspace session lifecycle feed is polled while the active
/// project has live children: the rows of that project are the ones moving at
/// this cadence.
const LIVE_PROJECT_POLL_MILLIS: u64 = 1_000;

/// How often the lifecycle feed is polled otherwise.
///
/// This cadence is what makes the project tree follow another client: a session
/// that `loom-cli` or a second client creates, renames, re-states, or archives
/// appears here within this window instead of after the next restart. The feed
/// carries catalog events only, so the choice is about how quickly the tree
/// should move rather than about what a poll costs.
const WORKSPACE_FEED_POLL_MILLIS: u64 = 2_000;

impl LoomView {
    /// Delays a poll while the worker is unreachable.
    ///
    /// A poll against a healthy worker keeps the caller's cadence, but a poll
    /// that is failing because the connection is gone waits the reconnect
    /// schedule's next delay instead. The retries therefore back off with the
    /// reconnect rather than hammering a worker that is down, and a transient
    /// failure is retried before anything is shown to the user.
    pub(crate) fn poll_delay_millis(&self, base_millis: u64) -> u64 {
        match self.reconnecting.as_ref() {
            Some(state) => state.schedule().next_delay_millis().max(base_millis),
            None => base_millis,
        }
    }

    /// Whether this client can follow the active workspace's session catalog.
    ///
    /// The tree lists the sessions of the worker the open session belongs to,
    /// and another client — `loom-cli`, or a second client on the same worker —
    /// can change that catalog at any moment. The feed is therefore followed
    /// whenever such a session is open, and not only while this client manages a
    /// project with live children as it used to be. The demo workspace has no
    /// worker, and a connection waiting for a sign-in is refused the workspace
    /// and session reads the feed is derived from, so neither is followed.
    pub(crate) fn workspace_feed_should_poll(&self) -> bool {
        !self.demo_workspace
            && self.is_connected()
            && !self.awaits_sign_in()
            && self
                .sessions
                .iter()
                .any(|session| session.id == self.active_session.id)
            && self.node_backends.contains_key(self.active_node_id())
    }

    /// The cadence of one lifecycle feed poll: the fastest that applies.
    pub(crate) fn workspace_feed_poll_delay_millis(&self) -> u64 {
        if self.project_root_is_active() && self.project_has_live_children() {
            LIVE_PROJECT_POLL_MILLIS
        } else {
            WORKSPACE_FEED_POLL_MILLIS
        }
    }

    /// Follows the active workspace's session catalog.
    ///
    /// The tree renders sessions another client created, so it cannot be built
    /// once at startup: the catalog is read as a feed, exactly like the open
    /// session's own event stream, and each poll applies what changed. The
    /// single-flight flag makes repeated calls from the render path free.
    pub(crate) fn schedule_workspace_feed_poll(&mut self, cx: &mut Context<Self>) {
        if self.workspace_feed_poll_scheduled || !self.workspace_feed_should_poll() {
            return;
        }
        self.workspace_feed_poll_scheduled = true;
        let delay = self.poll_delay_millis(self.workspace_feed_poll_delay_millis());
        cx.spawn(async move |view, cx| {
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
            // The cadence waits on the executor timer instead of a blocking
            // sleep, so following the catalog holds no background thread while
            // it waits.
            #[cfg(not(target_family = "wasm"))]
            cx.background_executor()
                .timer(Duration::from_millis(delay))
                .await;
            view.update(cx, |view, cx| {
                if view.workspace_feed_should_poll() {
                    view.poll_workspace_feed_once(cx);
                } else {
                    view.workspace_feed_poll_scheduled = false;
                }
            })
            .ok();
        })
        .detach();
    }

    /// Applies one page of the workspace session lifecycle feed, then follows
    /// the page it named.
    ///
    /// The response is the catalog's delta, so a page that reached the head of
    /// the feed also carries the head sequence: the cursor advances past the
    /// events this subscription filters out, and the next poll asks only for
    /// what happened after this one.
    pub(crate) fn poll_workspace_feed_once(&mut self, cx: &mut Context<Self>) {
        self.workspace_feed_poll_scheduled = true;
        let node_id = self.active_node_id().to_owned();
        let workspace_id = self.active_session.workspace_id;
        let request_cursor = self.workspace_feed_after_sequence;
        let stream_epoch = self.workspace_feed_epoch.clone();
        self.dispatch_to_node(
            cx,
            node_id.clone(),
            ClientRequest::Events(EventsRequest::GetWorkspaceSessionEvents {
                workspace_id,
                after_sequence: request_cursor,
                stream_epoch,
            }),
            move |view, response, cx| {
                // Read before the response is consumed, so the next page is
                // followed from the cursor this poll asked on.
                let continuation = workspace_lifecycle_page_continuation(&response, request_cursor);
                // A page that named one is drained before the cadence waits
                // again; the head sequence carries the cursor otherwise.
                let next_page = continuation.map(|(next, _)| next);
                match response.result {
                    Ok(ServerResponse::Events(EventsResponse::WorkspaceSessionEvents {
                        workspace_id: response_workspace,
                        events,
                        sessions,
                        latest_sequence,
                        stream_epoch,
                        ..
                    })) if response_workspace == workspace_id => {
                        view.workspace_feed_epoch = stream_epoch;
                        let reload = view.apply_workspace_lifecycle_events(&node_id, events, cx);
                        // A catalog in the answer means this client cannot be
                        // brought up to date from events alone, so it is applied
                        // after the batch: the catalog is the state the workspace
                        // is in now, and an event retained from before it must not
                        // win over that.
                        if let Some(catalog) = sessions
                            && view.apply_workspace_session_catalog(&node_id, workspace_id, catalog)
                        {
                            // A session this client has never listed may belong
                            // to a project, so read the ones no known project
                            // covers yet; a delegated child has to nest under
                            // the project that delegated it.
                            view.refresh_missing_project_snapshots(cx);
                        }
                        // The head of the feed carries the cursor past the
                        // events this subscription filtered out; a page that
                        // named a continuation advances to that instead.
                        view.advance_workspace_feed_cursor(latest_sequence, next_page);
                        if reload {
                            // A restored session carries no snapshot in its
                            // event, so the list is read again.
                            view.reload_sessions(cx);
                        }
                    }
                    Err(error) => view.record_backend_error("workspace event stream", error),
                    Ok(response) => view.record_backend_error(
                        "workspace event stream",
                        unexpected_response("workspace event stream", response),
                    ),
                }
                // The open project's snapshot is the only place a delegated
                // agent's record is rendered from, so an event that invalidated
                // it is read back as a fresh snapshot.
                if view.project_snapshot_stale {
                    view.refresh_active_project_snapshot(cx);
                }
                // A manager that ended its turn while children still run is
                // woken by the server. Keep consuming the root session's events
                // alongside the feed so that wake becomes visible.
                if view.project_root_is_active()
                    && view.project_has_live_children()
                    && !view.run_is_active()
                {
                    // Share the session poll's single-flight flag: a feed poll
                    // must not dispatch a second session poll while one is
                    // already in flight, because both would apply the same
                    // event batch.
                    view.start_run_poll_if_idle(cx);
                }
                // A poll for a workspace the client has since left is the last
                // one that workspace answers: the next poll starts from its new
                // cursor.
                if view.active_session.workspace_id != workspace_id {
                    view.workspace_feed_after_sequence = None;
                    view.workspace_feed_epoch = None;
                }
                match next_page {
                    // Keep draining the pages the backend named without waiting
                    // out the poll cadence; every answer advances the cursor, so
                    // the loop ends at the latest event of the feed.
                    Some(_) => view.poll_workspace_feed_once(cx),
                    None => {
                        view.workspace_feed_poll_scheduled = false;
                        view.schedule_workspace_feed_poll(cx);
                    }
                }
            },
        );
    }

    /// Advances the feed cursor to the highest sequence the answer established.
    ///
    /// The cursor never moves backwards: a page's continuation and the head the
    /// same page reported can arrive in either order, and a stale answer must
    /// not make the next poll ask for events this client already applied.
    pub(crate) fn advance_workspace_feed_cursor(
        &mut self,
        latest_sequence: EventSequence,
        next_after_sequence: Option<EventSequence>,
    ) {
        let cursor = next_after_sequence.unwrap_or(latest_sequence);
        self.workspace_feed_after_sequence = Some(
            self.workspace_feed_after_sequence
                .map_or(cursor, |current| current.max(cursor)),
        );
    }

    /// Applies newly journaled session events through the connection worker.
    ///
    /// The backend bounds an events batch to one response frame, so a poll that
    /// finds more events than fit keeps asking for the pages that follow instead
    /// of leaving the rest of the backlog for the next poll cadence.
    pub(crate) fn poll_run_once(&mut self, cx: &mut Context<Self>) {
        self.run_poll_scheduled = true;
        let request_cursor = self.after_sequence;
        let activity_detail = self.output_detail.activity_detail(self.show_reasoning);
        self.dispatch(
            cx,
            ClientRequest::Events(EventsRequest::GetSessionEvents {
                activity_detail,
                session_id: Some(self.active_session.id),
                workspace_id: None,
                after_sequence: request_cursor,
                stream_epoch: self.event_stream_epoch.clone(),
            }),
            move |view, response, cx| {
                // Read before the response is consumed, so the page the backend
                // names is followed on the cursor this poll answered.
                let continuation = events_page_continuation(&response, request_cursor);
                match response.result {
                    Ok(ServerResponse::Events(EventsResponse::SessionEvents {
                        events,
                        stream_epoch,
                        ..
                    })) => {
                        view.event_stream_epoch = stream_epoch;
                        // Applying by sequence makes the incremental poll
                        // idempotent: a re-delivered or overlapping batch
                        // contributes nothing because every event at or below
                        // the cursor has already been consumed.
                        view.apply_event_batch(events, None);
                    }
                    Ok(ServerResponse::Events(EventsResponse::SessionEventsSnapshot {
                        session,
                        stream_epoch,
                        ..
                    })) => {
                        // The snapshot's window can be a partial tail of the
                        // journal: `oldest_sequence` says how much older
                        // history the response left out. The incremental cursor
                        // is therefore no longer trustworthy, so rebuild the
                        // conversation from durable state exactly like an
                        // interactive session load (the run projection and the
                        // events after its cursor, then the newest transcript
                        // page) instead of replaying the tail over a cleared
                        // projection, which used to collapse the conversation
                        // to whatever the snapshot happened to carry.
                        view.event_stream_epoch = stream_epoch;
                        view.active_session = session;
                        view.reset_projection();
                        let session_id = view.active_session.id;
                        view.begin_session_data_load(session_id, cx);
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
                match continuation {
                    // Keep draining the pages the backend named without waiting
                    // for the next poll cadence; each answer advances the cursor,
                    // so the loop ends at the latest journaled event.
                    Some((next, _)) => {
                        view.after_sequence = Some(next);
                        view.poll_run_once(cx);
                    }
                    None => {
                        view.run_poll_scheduled = false;
                        view.schedule_run_poll(cx);
                    }
                }
            },
        );
    }

    pub(crate) fn run_is_active(&self) -> bool {
        matches!(
            self.run_state,
            Some(AgentRunState::Planning | AgentRunState::Executing | AgentRunState::Evaluating)
        )
    }

    /// Whether this client's own run keeps the stream polled.
    ///
    /// A run that is parked on a durable project join reports `Paused`, but the
    /// server resumes it when its children finish, so polling must continue
    /// past the active states or the wait never appears to complete.
    pub(crate) fn run_should_poll(&self) -> bool {
        self.run_is_active() || matches!(self.run_state, Some(AgentRunState::Paused))
    }

    /// Whether the session event stream should keep being polled.
    ///
    /// The protocol keeps the event stream resumable, so polling is sufficient
    /// for both local and remote connections without making the UI depend on a
    /// transport-specific push implementation.
    ///
    /// A run this client started is followed until it settles, and that used to
    /// be the whole rule. It left every other person's run invisible: their
    /// events are journaled all the same, but a client with no run of its own
    /// never asked for them, so the transcript of an open session stayed frozen
    /// while a colleague's run streamed into the journal. The stream of a
    /// session another client can write to is therefore followed while it is
    /// idle as well.
    pub(crate) fn session_events_should_poll(&self) -> bool {
        self.run_should_poll() || self.follows_shared_session()
    }

    /// Whether another client can add events to the open session.
    ///
    /// A worker reached at an address serves anybody who can reach that address,
    /// including a worker installed on this machine, and the demo workspace has
    /// no worker at all. A connection waiting for a sign-in may not read a
    /// session, and the placeholder this view holds before the first session is
    /// listed has no stream to read, so neither is followed.
    pub(crate) fn follows_shared_session(&self) -> bool {
        !self.demo_workspace
            && self.is_connected()
            && !self.awaits_sign_in()
            && self.active_worker_is_shared()
            && self
                .sessions
                .iter()
                .any(|session| session.id == self.active_session.id)
    }

    /// Whether the active session's worker was reached at an address.
    ///
    /// A worker installed on this machine is still a server: another client on
    /// the same machine can reach it at its loopback address. A `--remote`
    /// endpoint and every configured worker node are servers too, and another
    /// person can act on a session there at any moment.
    pub(crate) fn active_worker_is_shared(&self) -> bool {
        let node_id = self.active_node_id();
        if node_id == self.default_backend_node_id.as_str() {
            return self.backend_endpoint.is_some();
        }
        self.worker_nodes
            .iter()
            .any(|node| node.status.node_id == node_id && node.url.is_some())
    }

    /// The cadence of one session event poll: the fastest that applies.
    pub(crate) fn session_poll_delay_millis(&self) -> u64 {
        if self.run_is_active() {
            ACTIVE_RUN_POLL_MILLIS
        } else if matches!(self.run_state, Some(AgentRunState::Paused)) {
            PAUSED_RUN_POLL_MILLIS
        } else {
            IDLE_SESSION_POLL_MILLIS
        }
    }

    pub(crate) fn schedule_run_poll(&mut self, cx: &mut Context<Self>) {
        if self.run_poll_scheduled || !self.session_events_should_poll() {
            return;
        }
        self.run_poll_scheduled = true;
        let delay = self.poll_delay_millis(self.session_poll_delay_millis());
        cx.spawn(async move |view, cx| {
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
            // The cadence waits on the executor timer instead of a blocking
            // sleep, so following a session holds no background thread while it
            // waits.
            #[cfg(not(target_family = "wasm"))]
            cx.background_executor()
                .timer(Duration::from_millis(delay))
                .await;
            view.update(cx, |view, cx| {
                if !view.session_events_should_poll() {
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
        if self.active_run_id.is_none() {
            return;
        }
        self.start_run_poll_if_idle(cx);
    }

    /// Dispatches one session poll unless another poll is already scheduled.
    ///
    /// Callers that poll outside the scheduled cadence go through this guard,
    /// so a second, overlapping session poll cannot apply the same event batch
    /// twice.
    pub(crate) fn start_run_poll_if_idle(&mut self, cx: &mut Context<Self>) {
        if self.run_poll_scheduled {
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
        let remote_options = self.remote_options.clone();
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
            let remote_options = remote_options.clone();
            cx.spawn(async move |view, cx| {
                let result = cx
                    .background_spawn(async move {
                        let credentials = PeerCredentialStore::new();
                        let token = match credentials.get(workspace_id, &candidate_url) {
                            Ok(Some(token)) => token,
                            // No saved credential means this node was connected
                            // unsigned; reconnect it the same way so it can
                            // offer a GitHub sign-in again instead of failing.
                            Ok(None) => String::new(),
                            Err(error) => {
                                return Err((WorkerConnectionStage::CredentialRead, error, false));
                            }
                        };
                        let unsigned = crate::credential_store::connects_unsigned(&candidate_url, &token);
                        // The GitHub sign-in the client kept for this worker:
                        // a reconnect without an operator token replays it, and
                        // a worker that refuses it is retried once without.
                        let identity_key =
                            crate::identity_store::worker_key(Some(candidate_url.as_str()));
                        let connection = connect_remote(
                            &candidate_url,
                            &token,
                            &remote_options,
                            &identity_key,
                        )
                        .map_err(|error| {
                            (
                                transport_failure_stage(&candidate_url),
                                error,
                                false,
                            )
                        })?;
                        let negotiation = match negotiate(&connection) {
                            Ok(negotiation) => negotiation,
                            Err(error) => {
                                let cleanup_failed = connection.close().is_err();
                                return Err((
                                    WorkerConnectionStage::Negotiation,
                                    error,
                                    cleanup_failed,
                                ));
                            }
                        };
                        let status = match worker_node_status(&connection) {
                            Ok(status) => status,
                            Err(error) => {
                                let cleanup_failed = connection.close().is_err();
                                return Err((WorkerConnectionStage::Status, error, cleanup_failed));
                            }
                        };
                        let pre_login = unsigned
                            && current_participant(&connection)
                                .map(|participant| participant.is_none())
                                .unwrap_or(false);
                        // The remembered identity is stored under the address
                        // this worker is reached at, so it survives the worker
                        // restarting under a new instance id.
                        // A connection accepted without a credential cannot
                        // resume a person, so the stored participant is kept as
                        // a hint for the sign-in prompt instead.
                        let saved = crate::identity_store::load(&identity_key);
                        let mut remembered = None;
                        if unsigned {
                            remembered = saved;
                        } else if let Some(saved) = saved {
                            match resume_github_identity(&connection, saved.id) {
                                Ok(Some(_)) => {}
                                Ok(None) => crate::identity_store::clear(&identity_key),
                                Err(error) => log::debug!(
                                    "[loom-ui] could not resume the GitHub identity for worker \
                                     '{}': {}",
                                    identity_key,
                                    error.message
                                ),
                            }
                        }
                        Ok::<_, (WorkerConnectionStage, LoomError, bool)>((
                            connection,
                            status,
                            candidate_url,
                            pre_login,
                            unsigned,
                            remembered,
                            negotiation
                                .capabilities
                                .contains(loom_core::Capability::GitHubSignIn),
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
                        Ok((
                            connection,
                            status,
                            connected_url,
                            pre_login,
                            unsigned,
                            remembered,
                            offers_sign_in,
                        )) if is_pending => {
                            view.github_sign_in_available |= offers_sign_in;
                            // Marked before the node is attached, because
                            // attaching reloads every connected worker's
                            // sessions and a pre-login connection is refused
                            // that read.
                            if pre_login {
                                view.mark_node_pre_login(status.node_id.clone());
                            } else if unsigned {
                                view.mark_node_unsigned(status.node_id.clone());
                            }
                            if let Some(identity) = remembered {
                                view.remember_identity(status.node_id.clone(), identity);
                            }
                            view.attach_reconnected_worker_node(
                                id,
                                &connected_url,
                                connection,
                                status,
                                cx,
                            );
                        }
                        Ok((connection, _, _, _, _, _, _)) => {
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
                let mut answered = false;
                if let Some(node_index) = view.worker_nodes.iter().position(|node| node.id == id) {
                    let status_message = match results {
                        Ok(status) => {
                            answered = true;
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
                    if answered {
                        // A status answer means the connection works again,
                        // which ends any reconnecting state.
                        view.note_worker_answered(cx);
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
