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
                // Then choose a project title for the work that was just
                // recorded. This runs after the event was applied so the plan
                // and goal the run holds are the ones the event produced, and
                // it reads them from the cached state instead of the runtime,
                // whose lock the run loop already holds.
                match event {
                    AgentEvent::PlanProposed { plan, .. } => {
                        let goal = handle.state().run.task;
                        backend.maybe_auto_title(session_id, plan.title.as_deref(), &goal);
                    }
                    AgentEvent::RunCompleted { .. } => {
                        // A run that proposed a title already named its project,
                        // so the heuristic only covers runs that never proposed
                        // a plan and never overwrites a model title.
                        let state = handle.state();
                        if state.plan.title.is_none() {
                            backend.maybe_auto_title(session_id, None, &state.run.task);
                        }
                    }
                    _ => {}
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

    /// Settles a run whose admission deadline elapsed before it got a slot.
    /// Records the real admission error as a failed run instead of a user
    /// cancellation, so the transcript and session state say why.
    pub(crate) fn settle_rejected_run(self: &Arc<Self>, handle: &Arc<RunHandle>, error: LoomError) {
        log::warn!("could not admit run {}: {}", handle.run_id, error.message);
        handle.record_failure(error.clone());
        if let Ok(mut runtime) = handle.try_runtime() {
            if let Err(publish_error) = runtime.fail(error.message.clone()) {
                handle.record_failure(publish_error);
            }
            handle.refresh(&runtime);
        }
        if let Err(persist_error) = self.persist_worker_state() {
            handle.record_failure(persist_error);
        }
        handle.set_running(false);
    }

    /// Resolves how long a fresh run for this session may wait for a free
    /// concurrency slot, using the workspace's configured queue timeout.
    pub(crate) fn run_admission_timeout(&self, session_id: AgentSessionId) -> std::time::Duration {
        let workspace_id = self
            .sessions()
            .ok()
            .and_then(|sessions| sessions.get(session_id).ok())
            .map(|session| session.workspace_id);
        let seconds = workspace_id
            .and_then(|workspace_id| {
                self.workspace_configs().ok().and_then(|configs| {
                    configs
                        .get(&workspace_id)
                        .map(|config| config.run_admission_timeout_secs)
                })
            })
            .unwrap_or(loom_protocol::DEFAULT_RUN_ADMISSION_TIMEOUT_SECS);
        std::time::Duration::from_secs(u64::from(seconds.max(1)))
    }

    /// Drives a registered run on the bounded run executor so the request
    /// handler returns as soon as the run is registered. A run no longer owns a
    /// dedicated OS thread: at most `DEFAULT_MAX_CONCURRENT_RUNS` runs execute
    /// at once and excess runs wait for a slot.
    pub(crate) fn spawn_run_worker(self: &Arc<Self>, handle: Arc<RunHandle>) -> Result<()> {
        handle.join_worker()?;
        handle.set_running(true);
        let backend = Arc::clone(self);
        let reject_backend = Arc::clone(self);
        let task_handle = Arc::clone(&handle);
        let reject_handle = Arc::clone(&handle);
        let run_id = handle.run_id;
        let admission_timeout = self.run_admission_timeout(handle.session_id);
        let worker = self.run_executor.spawn_bounded(
            format!("loom-run-{run_id}"),
            admission_timeout,
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
                    // A model switch accepted while the previous turn was in
                    // flight takes effect here, before the next request is
                    // assembled from the provider.
                    if let Some(switch) = task_handle.take_model_switch() {
                        let PendingModelSwitch {
                            model,
                            provider,
                            pin,
                            input_cost_micros_per_1k,
                            output_cost_micros_per_1k,
                        } = switch;
                        let applied = (|| -> Result<bool> {
                            let mut runtime = task_handle
                                .runtime
                                .lock()
                                .unwrap_or_else(PoisonError::into_inner);
                            let changed = runtime.set_model(
                                model.clone(),
                                provider,
                                input_cost_micros_per_1k,
                                output_cost_micros_per_1k,
                            )?;
                            task_handle.refresh(&runtime);
                            Ok(changed)
                        })();
                        match applied {
                            Ok(true) => {
                                if let Err(error) =
                                    backend.pin_run_credential(task_handle.run_id, pin)
                                {
                                    task_handle.record_failure(error);
                                    break;
                                }
                                let event = AgentEvent::ModelChanged {
                                    run_id: task_handle.run_id,
                                    model,
                                };
                                if let Err(error) =
                                    backend.record_agent_event(task_handle.session_id, event)
                                {
                                    task_handle.record_failure(error);
                                    break;
                                }
                            }
                            Ok(false) => {}
                            Err(error) => {
                                task_handle.record_failure(error);
                                break;
                            }
                        }
                    }
                    let progress = {
                        let mut runtime = task_handle
                            .runtime
                            .lock()
                            .unwrap_or_else(PoisonError::into_inner);
                        // Re-read the GitHub credentials, write grant, and
                        // repository-access fact each step so settings toggled
                        // while a run is registered (for example while it waits
                        // for user input) take effect instead of keeping the
                        // values captured at run start.
                        runtime.set_github_access(
                            backend.providers.github_account_token().ok(),
                            backend.providers.github_write_access(),
                            backend.providers.github_repository_token().is_ok(),
                        );
                        // The session owns the goal, so a goal set or replaced
                        // while this run is registered reaches its next model
                        // turn instead of waiting for a new run.
                        if let Ok(sessions) = backend.sessions()
                            && let Ok(session) = sessions.get(task_handle.session_id)
                        {
                            runtime.set_goal(session.goal.clone());
                        }
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
            move |error| reject_backend.settle_rejected_run(&reject_handle, error),
        );
        *handle.worker.lock().unwrap_or_else(PoisonError::into_inner) = Some(worker);
        Ok(())
    }
}
