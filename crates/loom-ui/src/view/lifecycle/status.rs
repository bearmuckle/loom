use super::*;

impl LoomView {
    pub(crate) fn record_status(&mut self, status: impl Into<String>) {
        self.status_banner = Some(SystemNote::status(status.into()));
        // A fresh status supersedes the reconnecting notice: either the view
        // has newer information than the failure, or the connection came back
        // and the automatic attempts are done.
        self.clear_connectivity_state();
    }

    /// Records an operational failure.
    ///
    /// A connectivity failure is not an operation error: the worker never
    /// answered, so naming the operation and the internal code tells the user
    /// nothing they can act on. Those failures are folded into the client's
    /// single reconnecting state, described in plain language, with the raw
    /// code and message kept as details and in the log. Every other failure
    /// keeps the `{operation} · {code}` banner, which is right when the worker
    /// answered about the operation.
    pub(crate) fn record_backend_error(&mut self, operation: &str, error: LoomError) {
        if let Some(failure) = classify_connectivity_failure(&error) {
            self.record_connectivity_failure(operation, failure, &error);
            return;
        }
        self.status_details_open = false;
        self.status_banner = Some(SystemNote {
            tone: SystemTone::Error,
            heading: Some(format!("{operation} · {}", error.code)),
            text: match crate::collaboration::provider_auth_recovery(&error) {
                Some(hint) => format!("{} {hint}", error.message),
                None => error.message.clone(),
            },
            details: Some(error_details(&error)),
            progress: None,
            // Nothing here can re-run the failed call, and a retry sentence
            // with no control behind it is the bug this replaces.
            retryable: false,
        });
    }

    /// Folds a connectivity failure into the one reconnecting state.
    ///
    /// Repeated failures update that state instead of replacing the banner, so
    /// a worker that stays down produces one plain-language state with a
    /// moving attempt indicator rather than a series of coded messages.
    pub(crate) fn record_connectivity_failure(
        &mut self,
        operation: &str,
        failure: ConnectivityFailure,
        error: &LoomError,
    ) {
        if let Some(state) = self.reconnecting.as_mut() {
            if state.failure() == failure {
                // The state and its attempt counter are already showing; the
                // failure is expected while the worker is unreachable, so only
                // the copy and the raw details follow it.
                log::debug!("[loom-ui] worker is still unreachable during {operation}");
            } else {
                log::warn!(
                    "[loom-ui] worker connectivity failure during {operation}: {}: {}",
                    error.code,
                    error.message
                );
            }
            state.update(failure, error);
        } else {
            log::warn!(
                "[loom-ui] worker connectivity failure during {operation}: {}: {}",
                error.code,
                error.message
            );
            self.reconnecting = Some(Reconnecting::new(failure, error));
        }
        self.show_reconnecting_notice();
    }

    /// Shows, or refreshes, the banner for the reconnecting state.
    ///
    /// The raw code and message stay in `details`, one disclosure away, so the
    /// headline carries no internal code and no nested transport text while
    /// the failure stays debuggable.
    fn show_reconnecting_notice(&mut self) {
        let Some(state) = self.reconnecting.as_ref() else {
            return;
        };
        self.status_banner = Some(SystemNote {
            tone: SystemTone::Error,
            heading: Some(state.failure().headline().to_owned()),
            text: state.text().to_owned(),
            details: Some(state.details().to_owned()),
            progress: Some(state.progress()),
            retryable: true,
        });
    }

    /// Schedules the next automatic reconnect attempt.
    ///
    /// Called after every request that could have failed and once per attempt,
    /// so a lost connection is retried with backoff and recovers without a
    /// click. One attempt is in flight at a time, and a spent schedule stops
    /// and leaves the banner's Retry control to the user.
    pub(crate) fn schedule_connectivity_retry(&mut self, cx: &mut Context<Self>) {
        if self.connectivity_attempt_in_flight {
            return;
        }
        let Some(delay) = self
            .reconnecting
            .as_mut()
            .and_then(Reconnecting::take_attempt)
        else {
            // Either there is no reconnecting state, or its automatic attempts
            // are spent; refreshing the notice shows the spent wording.
            self.show_reconnecting_notice();
            return;
        };
        self.connectivity_attempt_in_flight = true;
        self.show_reconnecting_notice();
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
            #[cfg(not(target_family = "wasm"))]
            cx.background_spawn(async move {
                std::thread::sleep(Duration::from_millis(delay));
            })
            .await;
            view.update(cx, |view, cx| {
                view.connectivity_attempt_in_flight = false;
                if view.reconnecting.is_some() {
                    view.run_connectivity_attempt(cx);
                }
            })
            .ok();
        })
        .detach();
    }

    /// Runs one reconnect attempt now.
    ///
    /// The scheduled timer and the banner's Retry control both come through
    /// here, so a retry the user asks for behaves exactly like an automatic
    /// one, and a successful attempt shows the normal workspace again.
    pub(crate) fn run_connectivity_attempt(&mut self, cx: &mut Context<Self>) {
        if self.reconnecting.is_none() {
            return;
        }
        log::info!("[loom-ui] attempting to restore the worker connection");
        self.restore_worker_connection(cx);
        self.show_reconnecting_notice();
        self.schedule_connectivity_retry(cx);
        cx.notify();
    }

    /// Re-establishes the transport this platform can restore on its own.
    #[cfg(target_family = "wasm")]
    fn restore_worker_connection(&mut self, cx: &mut Context<Self>) {
        // Re-runs the browser bootstrap, which reloads projects, sessions, and
        // models from the worker instead of from client state that may be
        // stale.
        self.reconnect_browser(cx);
    }

    #[cfg(not(target_family = "wasm"))]
    fn restore_worker_connection(&mut self, cx: &mut Context<Self>) {
        // The native client re-establishes its saved worker-node connections;
        // the in-process backend needs no reconnect, and a stalled `--remote`
        // main connection is reopened by restarting the client.
        self.reconnect_configured_worker_nodes(cx);
        self.reload_sessions(cx);
    }

    /// Runs the status banner's Retry control.
    ///
    /// Only the reconnecting notice offers the control, so it restarts the
    /// schedule and attempts immediately: a spent schedule can run again, and
    /// a running one is asked to try now instead of waiting out its backoff.
    pub(crate) fn retry_status_action(&mut self, cx: &mut Context<Self>) {
        let Some(state) = self.reconnecting.as_mut() else {
            return;
        };
        state.reset_schedule();
        self.connectivity_attempt_in_flight = false;
        self.run_connectivity_attempt(cx);
    }

    /// Folds a worker answer into the reconnecting state.
    ///
    /// The first answer after the connection was lost ends the state and says
    /// so, so a recovery the client performed on its own is visible rather
    /// than silent.
    pub(crate) fn note_worker_answered(&mut self, cx: &mut Context<Self>) {
        self.connectivity_attempt_in_flight = false;
        if self.reconnecting.take().is_none() {
            return;
        }
        self.browser_connection_lost = None;
        log::info!("[loom-ui] worker connection restored");
        self.record_status("Worker connection restored");
        cx.notify();
    }

    /// Forgets the reconnecting state, which stops the automatic attempts.
    pub(crate) fn clear_connectivity_state(&mut self) {
        self.reconnecting = None;
        self.connectivity_attempt_in_flight = false;
    }

    /// The attempt indicator the disconnected screen shows while the client is
    /// reconnecting on its own.
    #[cfg(any(target_family = "wasm", test))]
    pub(crate) fn reconnect_progress(&self) -> Option<String> {
        self.reconnecting.as_ref().map(Reconnecting::progress)
    }

    pub(crate) fn dismiss_status_banner(&mut self, cx: &mut Context<Self>) {
        self.status_banner = None;
        self.status_details_open = false;
        // Taking the notice off screen also stops the automatic attempts; the
        // next failed request starts a fresh state.
        self.clear_connectivity_state();
        cx.notify();
    }

    /// Expands or collapses the status banner's raw details.
    pub(crate) fn toggle_status_details(&mut self, cx: &mut Context<Self>) {
        self.status_details_open = !self.status_details_open;
        cx.notify();
    }
}
