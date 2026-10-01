use super::*;

impl RunHandle {
    pub(crate) fn new(runtime: AgentRuntime) -> Self {
        let initial_state = runtime.export_state();
        let message_checkpoint = MessageCheckpointCursor {
            attempt_id: initial_state.run.attempt_id,
            // The handle cannot distinguish a newly-created run from a restored
            // one without touching SQLite. Start at zero so its first checkpoint
            // writes the complete current transcript; later checkpoints are tails.
            message_count: 0,
        };
        Self {
            run_id: runtime.run_id(),
            session_id: runtime.session_id(),
            control: runtime.control(),
            state: Mutex::new(initial_state),
            message_fragments: Mutex::new(MessageFragmentState::default()),
            activity_deltas: Mutex::new(PendingActivityDeltas::default()),
            message_checkpoint: Mutex::new(message_checkpoint),
            event_gate: Mutex::new(()),
            fragment_wake: Condvar::new(),
            runtime: Mutex::new(runtime),
            running: Mutex::new(false),
            idle: Condvar::new(),
            failure: Mutex::new(None),
            worker: Mutex::new(None),
        }
    }

    pub(crate) fn state(&self) -> AgentRuntimeState {
        self.locked_state().clone()
    }

    pub(crate) fn snapshot(&self) -> AgentRunSnapshot {
        self.locked_state().run.clone()
    }

    pub(crate) fn snapshot_projection(&self, include_messages: bool) -> AgentRunSnapshotProjection {
        run_snapshot_projection_with_messages(&self.locked_state(), include_messages)
    }

    pub(crate) fn locked_state(&self) -> MutexGuard<'_, AgentRuntimeState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub(crate) fn refresh(&self, runtime: &AgentRuntime) {
        let state = runtime.export_state();
        let mut fragments = self
            .message_fragments
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        *self.locked_state() = state;
        fragments.active_message_ordinal = None;
        self.fragment_wake.notify_all();
    }

    pub(crate) fn append_message_delta(
        &self,
        persistence: &dyn Persistence,
        text: &str,
    ) -> Result<()> {
        if text.is_empty() {
            return Ok(());
        }
        let mut fragments = self.message_fragments.lock().map_err(|_| {
            LoomError::new(
                ErrorCode::Internal,
                "run message fragment lock was poisoned",
                true,
            )
        })?;
        let ordinal = match fragments.active_message_ordinal {
            Some(ordinal) => ordinal,
            None => {
                let state = self.locked_state();
                let message_count = u64::try_from(state.messages.len()).map_err(|_| {
                    LoomError::new(
                        ErrorCode::Persistence,
                        "run transcript has too many messages",
                        false,
                    )
                })?;
                let ordinal = if state
                    .messages
                    .last()
                    .is_some_and(|message| message.role == loom_model::MessageRole::Assistant)
                {
                    message_count.saturating_sub(1)
                } else {
                    message_count
                };
                fragments.active_message_ordinal = Some(ordinal);
                ordinal
            }
        };
        if let std::collections::btree_map::Entry::Vacant(entry) =
            fragments.positions.entry(ordinal)
        {
            let (fragment_ordinal, byte_offset) =
                persistence.next_run_message_fragment_position(self.run_id, ordinal)?;
            entry.insert(MessageFragmentPosition {
                ordinal,
                fragment_ordinal,
                byte_offset,
            });
        }
        let pending_bytes = fragments
            .pending_bytes
            .checked_add(text.len())
            .ok_or_else(|| {
                LoomError::new(
                    ErrorCode::Persistence,
                    "pending run message content is too large",
                    false,
                )
            })?;
        let now = Instant::now();
        if fragments.pending_bytes == 0 {
            fragments.pending_since = Some(now);
        }
        fragments
            .pending
            .entry(ordinal)
            .or_default()
            .content
            .push_str(text);
        fragments.pending_bytes = pending_bytes;
        let interval_elapsed = fragments.pending_since.is_some_and(|pending_since| {
            now.saturating_duration_since(pending_since) >= MESSAGE_FRAGMENT_BATCH_INTERVAL
        });
        if fragments.pending_bytes >= MESSAGE_FRAGMENT_BATCH_BYTES || interval_elapsed {
            self.flush_message_fragments_locked(persistence, &mut fragments)?;
        }
        self.fragment_wake.notify_one();
        Ok(())
    }

    pub(crate) fn flush_message_fragments(&self, persistence: &dyn Persistence) -> Result<()> {
        let mut fragments = self.message_fragments.lock().map_err(|_| {
            LoomError::new(
                ErrorCode::Internal,
                "run message fragment lock was poisoned",
                true,
            )
        })?;
        self.flush_message_fragments_locked(persistence, &mut fragments)
    }

    pub(crate) fn flush_message_fragments_locked(
        &self,
        persistence: &dyn Persistence,
        fragments: &mut MessageFragmentState,
    ) -> Result<()> {
        let ordinals = fragments.pending.keys().copied().collect::<Vec<_>>();
        for ordinal in ordinals {
            if let std::collections::btree_map::Entry::Vacant(entry) =
                fragments.positions.entry(ordinal)
            {
                let (fragment_ordinal, byte_offset) =
                    persistence.next_run_message_fragment_position(self.run_id, ordinal)?;
                entry.insert(MessageFragmentPosition {
                    ordinal,
                    fragment_ordinal,
                    byte_offset,
                });
            }
            loop {
                let (start, end, content) = {
                    let pending = fragments.pending.get(&ordinal).ok_or_else(|| {
                        LoomError::new(
                            ErrorCode::Internal,
                            "pending run message fragment buffer is missing",
                            false,
                        )
                    })?;
                    let start = pending.committed_bytes;
                    if start >= pending.content.len() {
                        break;
                    }
                    let mut end = (start + MESSAGE_FRAGMENT_BATCH_BYTES).min(pending.content.len());
                    while !pending.content.is_char_boundary(end) {
                        end -= 1;
                    }
                    (start, end, pending.content.as_bytes()[start..end].to_vec())
                };
                let position = fragments.positions.get(&ordinal).copied().ok_or_else(|| {
                    LoomError::new(
                        ErrorCode::Internal,
                        "streamed message fragment cursor is missing",
                        false,
                    )
                })?;
                let next_fragment_ordinal =
                    position.fragment_ordinal.checked_add(1).ok_or_else(|| {
                        LoomError::new(
                            ErrorCode::Persistence,
                            "run message has too many persisted fragments",
                            false,
                        )
                    })?;
                let next_byte_offset = position
                    .byte_offset
                    .checked_add(u64::try_from(content.len()).map_err(|_| {
                        LoomError::new(
                            ErrorCode::Persistence,
                            "run message fragment length is out of range",
                            false,
                        )
                    })?)
                    .ok_or_else(|| {
                        LoomError::new(
                            ErrorCode::Persistence,
                            "run message content is too large",
                            false,
                        )
                    })?;
                persistence.append_run_message_fragment(
                    self.run_id,
                    self.session_id,
                    position.ordinal,
                    position.fragment_ordinal,
                    position.byte_offset,
                    &content,
                )?;
                let position = fragments.positions.get_mut(&ordinal).ok_or_else(|| {
                    LoomError::new(
                        ErrorCode::Internal,
                        "streamed message fragment cursor is missing",
                        false,
                    )
                })?;
                position.fragment_ordinal = next_fragment_ordinal;
                position.byte_offset = next_byte_offset;
                let pending = fragments.pending.get_mut(&ordinal).ok_or_else(|| {
                    LoomError::new(
                        ErrorCode::Internal,
                        "pending run message fragment buffer is missing",
                        false,
                    )
                })?;
                pending.committed_bytes = end;
                fragments.pending_bytes = fragments
                    .pending_bytes
                    .checked_sub(end - start)
                    .ok_or_else(|| {
                        LoomError::new(
                            ErrorCode::Internal,
                            "pending run message byte count is inconsistent",
                            false,
                        )
                    })?;
            }
            if fragments
                .pending
                .get(&ordinal)
                .is_some_and(|pending| pending.committed_bytes == pending.content.len())
            {
                fragments.pending.remove(&ordinal);
            }
        }
        if fragments.pending_bytes == 0 {
            fragments.pending_since = None;
        }
        Ok(())
    }

    pub(crate) fn flush_message_fragments_until_stopped(
        handle: Weak<Self>,
        persistence: Arc<dyn Persistence>,
    ) {
        loop {
            let Some(handle) = handle.upgrade() else {
                return;
            };
            let mut fragments = handle
                .message_fragments
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            if fragments.pending_bytes == 0 {
                if !handle.is_running() {
                    return;
                }
                let (guard, _) = handle
                    .fragment_wake
                    .wait_timeout(fragments, Duration::from_secs(1))
                    .unwrap_or_else(PoisonError::into_inner);
                fragments = guard;
                if fragments.pending_bytes == 0 && !handle.is_running() {
                    return;
                }
                continue;
            }
            let deadline = fragments
                .pending_since
                .map(|pending_since| pending_since + MESSAGE_FRAGMENT_BATCH_INTERVAL)
                .unwrap_or_else(Instant::now);
            let Some(wait) = deadline.checked_duration_since(Instant::now()) else {
                if let Err(error) =
                    handle.flush_message_fragments_locked(&persistence, &mut fragments)
                {
                    drop(fragments);
                    handle.record_failure(error);
                    handle.control.request_interrupt();
                    return;
                }
                continue;
            };
            let (guard, _) = handle
                .fragment_wake
                .wait_timeout(fragments, wait)
                .unwrap_or_else(PoisonError::into_inner);
            drop(guard);
        }
    }

    /// Keeps the cached run state current while a step is still executing.
    ///
    /// The transcript in the cached state is only replaced when the step ends;
    /// live message deltas are observable through the event journal.
    pub(crate) fn apply_event(&self, event: &AgentEvent) {
        let mut state = self.locked_state();
        match event {
            AgentEvent::RunStarted { snapshot } | AgentEvent::RunCompleted { snapshot } => {
                state.run = snapshot.clone();
                sync_cached_run_attempt(&mut state);
            }
            AgentEvent::RunStateChanged {
                state: run_state, ..
            } => {
                state.run.state = *run_state;
                state.run.updated_at = Timestamp::now();
                if !matches!(
                    run_state,
                    AgentRunState::AwaitingApproval | AgentRunState::Paused
                ) {
                    state.pending_approval = None;
                }
                if !matches!(run_state, AgentRunState::NeedsInput | AgentRunState::Paused) {
                    state.pending_input = None;
                }
                let checkpoint_retry_transition = *run_state == AgentRunState::Planning
                    && state.attempts.iter().any(|attempt| {
                        attempt.id == state.run.attempt_id && attempt.completed_at.is_some()
                    });
                if !checkpoint_retry_transition {
                    sync_cached_run_attempt(&mut state);
                }
            }
            AgentEvent::RunUsageUpdated { usage, .. } => state.usage = usage.clone(),
            AgentEvent::ToolApprovalRequired {
                attempt_id,
                control_revision,
                call,
                ..
            } => {
                state.pending_approval = Some(call.clone());
                state.run.attempt_id = *attempt_id;
                state.run.control_revision = *control_revision;
            }
            AgentEvent::ToolApprovalDecided {
                attempt_id,
                control_revision,
                ..
            } => {
                state.pending_approval = None;
                state.run.attempt_id = *attempt_id;
                state.run.control_revision = *control_revision;
            }
            AgentEvent::NeedsInput {
                attempt_id,
                control_revision,
                prompt,
                ..
            } => {
                state.pending_input = Some(prompt.clone());
                state.run.attempt_id = *attempt_id;
                state.run.control_revision = *control_revision;
            }
            AgentEvent::UserMessage {
                attempt_id,
                control_revision,
                ..
            } => {
                state.pending_input = None;
                state.run.attempt_id = *attempt_id;
                state.run.control_revision = *control_revision;
            }
            AgentEvent::ToolCallStarted { call, .. } => {
                if state
                    .pending_tool_execution
                    .as_ref()
                    .is_some_and(|pending| pending.id == call.id)
                {
                    state.pending_tool_execution = None;
                }
            }
            AgentEvent::ActivityRecorded { activity, .. } => {
                let updated = if let Some(existing) = state
                    .activities
                    .iter_mut()
                    .find(|existing| existing.id == activity.id)
                {
                    *existing = activity.clone();
                    true
                } else {
                    state.activities.push(activity.clone());
                    false
                };
                let mut deltas = self
                    .activity_deltas
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner);
                if !updated && !deltas.by_id.contains_key(&activity.id) {
                    deltas.appended_order.push(activity.id);
                }
                deltas.by_id.insert(activity.id, activity.clone());
            }
            _ => {}
        }
    }

    pub(crate) fn is_running(&self) -> bool {
        *self.running.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub(crate) fn set_running(&self, running: bool) {
        *self.running.lock().unwrap_or_else(PoisonError::into_inner) = running;
        self.idle.notify_all();
        self.fragment_wake.notify_all();
    }

    pub(crate) fn join_worker(&self) -> Result<()> {
        let worker = self
            .worker
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        if let Some(worker) = worker {
            worker.wait();
            if let Some(message) = worker.failure() {
                return Err(LoomError::new(
                    ErrorCode::Internal,
                    format!("agent run {} worker panicked: {message}", self.run_id),
                    false,
                ));
            }
        }
        Ok(())
    }

    pub(crate) fn failure(&self) -> Option<LoomError> {
        self.failure
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Locks the runtime for an operation that requires exclusive access.
    ///
    /// Returns a retryable conflict instead of blocking when a step is in
    /// flight, so a caller is never parked behind a model call.
    pub(crate) fn try_runtime(&self) -> Result<MutexGuard<'_, AgentRuntime>> {
        match self.runtime.try_lock() {
            Ok(runtime) => Ok(runtime),
            Err(std::sync::TryLockError::Poisoned(poisoned)) => Ok(poisoned.into_inner()),
            Err(std::sync::TryLockError::WouldBlock) => Err(LoomError::new(
                ErrorCode::Conflict,
                format!("agent run {} is executing a step", self.run_id),
                true,
            )),
        }
    }

    /// Locks the runtime for an operation that responds to a state the run has
    /// already reached, allowing the worker a moment to finish its last step.
    pub(crate) fn runtime_for_entry(&self) -> Result<MutexGuard<'_, AgentRuntime>> {
        // Approval events are journaled before the worker releases the runtime
        // lock. Give that transition the same settle time as other control
        // operations so a client can approve as soon as the prompt appears.
        let settle_timeout = if self.state().run.state == AgentRunState::AwaitingApproval {
            CONTROL_SETTLE_TIMEOUT
        } else {
            ENTRY_SETTLE_TIMEOUT
        };
        if self.is_running() && self.wait_until_idle_for(settle_timeout).is_err() {
            return Err(LoomError::new(
                ErrorCode::Conflict,
                format!("agent run {} is executing a step", self.run_id),
                true,
            ));
        }
        self.try_runtime()
    }

    /// Waits for an in-flight step to observe a pause or interrupt request.
    pub(crate) fn wait_until_idle(&self) -> Result<()> {
        self.wait_until_idle_for(CONTROL_SETTLE_TIMEOUT)
    }

    pub(crate) fn wait_until_idle_for(&self, timeout: Duration) -> Result<()> {
        let deadline = Instant::now() + timeout;
        let mut running = self.running.lock().unwrap_or_else(PoisonError::into_inner);
        while *running {
            let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
                return Err(LoomError::new(
                    ErrorCode::DeadlineExceeded,
                    format!("agent run {} did not stop in time", self.run_id),
                    true,
                ));
            };
            let (guard, timeout) = self
                .idle
                .wait_timeout(running, remaining)
                .unwrap_or_else(PoisonError::into_inner);
            running = guard;
            if timeout.timed_out() && *running {
                return Err(LoomError::new(
                    ErrorCode::DeadlineExceeded,
                    format!("agent run {} did not stop in time", self.run_id),
                    true,
                ));
            }
        }
        Ok(())
    }

    pub(crate) fn take_failure(&self) -> Option<LoomError> {
        self.failure
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
    }

    pub(crate) fn record_failure(&self, error: LoomError) {
        let mut failure = self.failure.lock().unwrap_or_else(PoisonError::into_inner);
        if failure.is_none() {
            *failure = Some(error);
        }
    }
}
