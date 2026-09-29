use super::*;

impl InProcessBackend {
    /// Journals one agent event and keeps the session state in step with it.
    pub(crate) fn record_agent_event(
        self: &Arc<Self>,
        session_id: AgentSessionId,
        event: AgentEvent,
    ) -> Result<()> {
        let state = session_state_for_event(&event);
        self.journal()?.append_agent(session_id, event);
        if let Some(state) = state {
            let current = self.sessions()?.get(session_id)?.state;
            if current != state {
                let (_, record) = self.sessions()?.transition(session_id, state)?;
                self.journal()?.append_session(record);
            }
        }
        Ok(())
    }

    pub(crate) fn after_run_checkpoint(self: &Arc<Self>, handle: &RunHandle) -> Result<()> {
        let state = handle.state().run.state;
        let session_id = handle.session_id;
        let session_state = session_state_for_run_state(state);
        if !project_agent_slot_released(session_state) {
            return Ok(());
        }
        self.update_project_task_for_session_state(session_id, session_state)?;
        self.reconcile_project_tasks_and_resume_queued(false)
    }

    /// Observer installed on every runtime so events are journaled as they are
    /// produced rather than after the run finishes.
    pub(crate) fn run_observer(
        self: &Arc<Self>,
        handle: Weak<RunHandle>,
        session_id: AgentSessionId,
    ) -> AgentEventObserver {
        let backend = Arc::downgrade(self);
        Arc::new(move |event: &AgentEvent| {
            let Some(backend) = backend.upgrade() else {
                return;
            };
            let handle = handle.upgrade();
            // Keep journal append and cached-state/dirty-activity application
            // indivisible relative to a durable worker checkpoint.
            let _event_guard = handle.as_ref().map(|handle| {
                handle
                    .event_gate
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
            });
            let fragment_result = match (event, backend.persistence.as_ref(), handle.as_ref()) {
                (
                    AgentEvent::AssistantMessageDelta { text, .. },
                    Some(persistence),
                    Some(handle),
                ) => handle.append_message_delta(persistence, text),
                (AgentEvent::RunCompleted { .. }, Some(persistence), Some(handle)) => {
                    handle.flush_message_fragments(persistence)
                }
                _ => Ok(()),
            };
            let recorded = fragment_result
                .and_then(|()| backend.record_agent_event(session_id, event.clone()));
            if let Some(handle) = handle.as_ref() {
                handle.apply_event(event);
                if let Err(error) = recorded {
                    handle.record_failure(error);
                }
            }
        })
    }

    /// Wraps a runtime in a handle and attaches the journaling observer.
    pub(crate) fn register_runtime(self: &Arc<Self>, mut runtime: AgentRuntime) -> Arc<RunHandle> {
        let session_id = runtime.session_id();
        Arc::new_cyclic(|weak: &Weak<RunHandle>| {
            runtime.set_event_observer(self.run_observer(weak.clone(), session_id));
            RunHandle::new(runtime)
        })
    }

    /// Drives a registered run on its own worker so the request handler returns
    /// as soon as the run is registered.
    pub(crate) fn spawn_run_worker(self: &Arc<Self>, handle: Arc<RunHandle>) -> Result<()> {
        handle.join_worker()?;
        handle.set_running(true);
        let backend = Arc::clone(self);
        let worker_handle = Arc::clone(&handle);
        let run_id = handle.run_id;
        let worker = thread::Builder::new()
            .name(format!("loom-run-{run_id}"))
            .spawn(move || {
                let fragment_flusher = backend.persistence.clone().map(|persistence| {
                    let handle = Arc::downgrade(&handle);
                    thread::spawn(move || {
                        RunHandle::flush_message_fragments_until_stopped(handle, persistence)
                    })
                });
                loop {
                    let delivered = if let Some(persistence) = backend.persistence.as_ref() {
                        let mut runtime = handle
                            .runtime
                            .lock()
                            .unwrap_or_else(PoisonError::into_inner);
                        match deliver_project_agent_messages(persistence, &mut runtime) {
                            Ok(delivered) => {
                                if delivered {
                                    handle.refresh(&runtime);
                                }
                                delivered
                            }
                            Err(error) => {
                                handle.record_failure(error);
                                false
                            }
                        }
                    } else {
                        false
                    };
                    if handle.failure().is_some() {
                        break;
                    }
                    if delivered && let Err(error) = backend.persist_run_checkpoint(&handle) {
                        handle.record_failure(error);
                        break;
                    }
                    let progress = {
                        let mut runtime = handle
                            .runtime
                            .lock()
                            .unwrap_or_else(PoisonError::into_inner);
                        // Re-read the GitHub credentials and write grant each step
                        // so settings toggled while a run is registered (for
                        // example while it waits for user input) take effect
                        // instead of keeping the values captured at run start.
                        runtime.set_github_access(
                            backend.providers.github_account_token().ok(),
                            backend.providers.github_write_access(),
                        );
                        let progress = runtime.run_step();
                        handle.refresh(&runtime);
                        progress
                    };
                    if handle.failure().is_some() {
                        break;
                    }
                    match progress {
                        Ok(progress) => {
                            if let Some(persistence) = backend.persistence.as_ref()
                                && let Err(error) = handle.flush_message_fragments(persistence)
                            {
                                handle.record_failure(error);
                                break;
                            }
                            if let Err(error) = backend.persist_run_checkpoint(&handle) {
                                handle.record_failure(error);
                                break;
                            }
                            if let Err(error) = backend.after_run_checkpoint(&handle) {
                                handle.record_failure(error);
                                break;
                            }
                            if !progress.continues {
                                break;
                            }
                        }
                        Err(error) => {
                            let flush_error =
                                backend.persistence.as_ref().and_then(|persistence| {
                                    handle.flush_message_fragments(persistence).err()
                                });
                            handle.record_failure(flush_error.unwrap_or(error));
                            break;
                        }
                    }
                }
                if let Err(error) = backend.persist_worker_state() {
                    handle.record_failure(error);
                }
                handle.set_running(false);
                if fragment_flusher.is_some_and(|flusher| flusher.join().is_err()) {
                    log::error!("run message fragment flusher thread panicked");
                }
            })
            .map_err(|error| {
                worker_handle.set_running(false);
                LoomError::new(
                    ErrorCode::Internal,
                    format!("could not start worker for run {run_id}: {error}"),
                    true,
                )
            })?;
        *worker_handle
            .worker
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(worker);
        Ok(())
    }
}
