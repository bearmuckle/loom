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
        self.reconcile_project_tasks_and_resume_queued(false)?;
        self.notify_parent_of_child_activity(session_id)
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

    /// Drives a registered run on the bounded run executor so the request
    /// handler returns as soon as the run is registered. A run no longer owns a
    /// dedicated OS thread: at most `DEFAULT_MAX_CONCURRENT_RUNS` runs execute
    /// at once and excess runs wait for a slot.
    pub(crate) fn spawn_run_worker(self: &Arc<Self>, handle: Arc<RunHandle>) -> Result<()> {
        handle.join_worker()?;
        handle.set_running(true);
        let backend = Arc::clone(self);
        let task_handle = Arc::clone(&handle);
        let reject_handle = Arc::clone(&handle);
        let run_id = handle.run_id;
        let worker = self.run_executor.spawn_bounded(
            format!("loom-run-{run_id}"),
            move || {
                let fragment_flusher = backend.persistence.clone().map(|persistence| {
                    let handle = Arc::downgrade(&task_handle);
                    backend.run_executor.spawn_blocking(
                        format!("loom-run-{run_id}-fragments"),
                        move || {
                            RunHandle::flush_message_fragments_until_stopped(handle, persistence)
                        },
                    )
                });
                loop {
                    let delivered = if let Some(persistence) = backend.persistence.as_ref() {
                        let mut runtime = task_handle
                            .runtime
                            .lock()
                            .unwrap_or_else(PoisonError::into_inner);
                        let project = deliver_project_agent_messages(persistence, &mut runtime);
                        let directions = deliver_queued_run_directions(persistence, &mut runtime);
                        let mut delivered = false;
                        for result in [project, directions] {
                            match result {
                                Ok(true) => delivered = true,
                                Ok(false) => {}
                                Err(error) => {
                                    task_handle.record_failure(error);
                                    break;
                                }
                            }
                        }
                        if delivered {
                            task_handle.refresh(&runtime);
                        }
                        delivered
                    } else {
                        false
                    };
                    if task_handle.failure().is_some() {
                        break;
                    }
                    if delivered && let Err(error) = backend.persist_run_checkpoint(&task_handle) {
                        task_handle.record_failure(error);
                        break;
                    }
                    let progress = {
                        let mut runtime = task_handle
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
                        task_handle.refresh(&runtime);
                        progress
                    };
                    if task_handle.failure().is_some() {
                        break;
                    }
                    match progress {
                        Ok(progress) => {
                            if let Some(persistence) = backend.persistence.as_ref()
                                && let Err(error) = task_handle.flush_message_fragments(persistence)
                            {
                                task_handle.record_failure(error);
                                break;
                            }
                            if let Err(error) = backend.persist_run_checkpoint(&task_handle) {
                                task_handle.record_failure(error);
                                break;
                            }
                            if let Err(error) = backend.after_run_checkpoint(&task_handle) {
                                task_handle.record_failure(error);
                                break;
                            }
                            if !progress.continues {
                                // A direction queued as the run stopped (for
                                // example while it was asking for input) is
                                // delivered here so it is not stranded until
                                // an unrelated resume.
                                let delivered = if let Some(persistence) =
                                    backend.persistence.as_ref()
                                {
                                    let mut runtime = task_handle
                                        .runtime
                                        .lock()
                                        .unwrap_or_else(PoisonError::into_inner);
                                    match deliver_queued_run_directions(persistence, &mut runtime) {
                                        Ok(delivered) => {
                                            if delivered {
                                                task_handle.refresh(&runtime);
                                            }
                                            delivered
                                        }
                                        Err(error) => {
                                            task_handle.record_failure(error);
                                            false
                                        }
                                    }
                                } else {
                                    false
                                };
                                if delivered {
                                    if let Err(error) = backend.persist_run_checkpoint(&task_handle)
                                    {
                                        task_handle.record_failure(error);
                                        break;
                                    }
                                    if let Err(error) = backend.after_run_checkpoint(&task_handle) {
                                        task_handle.record_failure(error);
                                        break;
                                    }
                                    continue;
                                }
                                break;
                            }
                        }
                        Err(error) => {
                            let flush_error =
                                backend.persistence.as_ref().and_then(|persistence| {
                                    task_handle.flush_message_fragments(persistence).err()
                                });
                            task_handle.record_failure(flush_error.unwrap_or(error));
                            break;
                        }
                    }
                }
                if let Err(error) = backend.persist_worker_state() {
                    task_handle.record_failure(error);
                }
                task_handle.set_running(false);
                if let Some(flusher) = fragment_flusher {
                    flusher.wait();
                    if let Some(message) = flusher.failure() {
                        log::error!("run {run_id} message fragment flusher panicked: {message}");
                    }
                }
            },
            move |error| {
                log::warn!("could not admit run {run_id}: {}", error.message);
                reject_handle.record_failure(error);
                // No worker owns the runtime yet, so settle the run here rather
                // than leaving it running with no driver.
                if let Ok(mut runtime) = reject_handle.try_runtime() {
                    let _ = runtime.interrupt();
                    reject_handle.refresh(&runtime);
                }
                reject_handle.set_running(false);
            },
        );
        *handle.worker.lock().unwrap_or_else(PoisonError::into_inner) = Some(worker);
        Ok(())
    }
}
