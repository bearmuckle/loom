use super::*;

impl InProcessBackend {
    pub(crate) fn set_workspace_config(
        self: &Arc<Self>,
        workspace_id: WorkspaceId,
        config: WorkspaceConfig,
    ) -> Result<()> {
        let unique_urls = config
            .worker_nodes
            .iter()
            .map(|node| node.url.as_str())
            .collect::<BTreeSet<_>>();
        if config.worker_nodes.len() > 64
            || unique_urls.len() != config.worker_nodes.len()
            || config
                .worker_nodes
                .iter()
                .any(|node| node.url.trim() != node.url || !worker_node_url_is_safe(&node.url))
            || !(loom_protocol::MIN_PROJECT_AGENT_CONCURRENCY
                ..=loom_protocol::MAX_PROJECT_AGENT_CONCURRENCY)
                .contains(&config.project_agent_concurrency)
        {
            return Err(LoomError::invalid_request(
                "workspace configuration must contain at most 64 safe worker-node URLs and project agent concurrency between 1 and 16",
            ));
        }
        let current_revision = self
            .workspace_configs()?
            .get(&workspace_id)
            .map(|config| config.revision);
        if current_revision.is_some_and(|revision| revision > config.revision) {
            return Ok(());
        }
        let previous_concurrency = self
            .workspace_configs()?
            .get(&workspace_id)
            .map(|config| config.project_agent_concurrency)
            .unwrap_or_else(|| WorkspaceConfig::default().project_agent_concurrency);
        let concurrency_changed = previous_concurrency != config.project_agent_concurrency;
        let revision = config.revision;
        let previous = self.workspace_configs()?.insert(workspace_id, config);
        let sequence = self
            .journal()?
            .append_workspace(workspace_id, WorkspaceEvent::ConfigChanged { revision });
        if let Err(error) = self.persist_state() {
            let mut configs = self.workspace_configs()?;
            if let Some(previous) = previous {
                configs.insert(workspace_id, previous);
            } else {
                configs.remove(&workspace_id);
            }
            self.journal()?.discard_pending_workspace(sequence);
            return Err(error);
        }
        if concurrency_changed {
            self.reconcile_project_tasks_and_resume_queued(false)?;
        }
        Ok(())
    }

    pub(crate) fn persist_state(&self) -> Result<()> {
        self.persist_state_with_recovery_updates(&BTreeMap::new())
    }

    pub(crate) fn persist_state_with_idempotency_candidate(
        &self,
        candidate: (loom_core::RequestId, IdempotencyRecord),
    ) -> Result<()> {
        self.latch_on_persistence_error(self.persist_state_inner(&BTreeMap::new(), Some(candidate)))
    }

    /// Persists the current worker checkpoint without enumerating unrelated runs,
    /// catalogs, or session filesystems. The journal lock is retained through the
    /// transaction so only the captured event prefix can be acknowledged.
    pub(crate) fn persist_run_checkpoint(&self, handle: &RunHandle) -> Result<()> {
        self.ensure_persistence_healthy()?;
        let result = self.persist_run_checkpoint_inner(handle);
        self.latch_on_persistence_error(result)
    }

    pub(crate) fn persist_worker_state(&self) -> Result<()> {
        self.ensure_persistence_healthy()?;
        self.persist_state()
    }

    pub(crate) fn ensure_persistence_healthy(&self) -> Result<()> {
        if self.persistence_failed.load(Ordering::SeqCst) {
            Err(LoomError::new(
                ErrorCode::Persistence,
                "backend is unavailable after a durable state save failure; reopen it to recover",
                true,
            ))
        } else {
            Ok(())
        }
    }

    pub(crate) fn persist_run_checkpoint_inner(&self, handle: &RunHandle) -> Result<()> {
        let Some(persistence) = &self.persistence else {
            return Ok(());
        };
        let _event_guard = handle
            .event_gate
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        handle.flush_message_fragments(persistence)?;
        // Project only the checkpoint fields while holding the state lock.
        // This intentionally excludes the potentially large activities vector;
        // changed rows come from the ID-keyed queue below.
        let (
            session_id,
            summary,
            runtime_config,
            context_checkpoint,
            plan,
            message_delta,
            activities,
            project_manager_wait,
        ) = {
            let state = handle.locked_state();
            let summary = DurableRunSummary {
                snapshot: state.run.clone(),
                usage: state.usage.clone(),
                attempts: Some(state.attempts.clone()),
                execution_state: Some(execution_state_from_runtime(&state)?),
                interactions: Some(state.interactions.clone()),
            };
            let mut context_inspection = state.context_inspection.clone();
            if let Some(inspection) = &mut context_inspection {
                inspection.summary = None;
            }
            let runtime_config = DurableRunRuntimeConfig {
                system_instructions: state.task.system_instructions.clone(),
                repository_instructions: state.task.repository_instructions.clone(),
                approval_policy: state.approval_policy.clone(),
                limits: state.options.limits.clone(),
                context_options: state.options.context.clone(),
                checkpoint_id: state.options.checkpoint_id,
                input_cost_micros_per_1k: state.options.input_cost_micros_per_1k,
                output_cost_micros_per_1k: state.options.output_cost_micros_per_1k,
                context_inspection,
                project_delegation_enabled: state.options.project_delegation_enabled,
                project_messaging_enabled: state.options.project_messaging_enabled,
                project_inspection_enabled: state.options.project_inspection_enabled,
                project_child_control_enabled: state.options.project_child_control_enabled,
                project_worktree_enabled: state.options.project_worktree_enabled,
                project_review_enabled: state.options.project_review_enabled,
                project_integration_enabled: state.options.project_integration_enabled,
                project_branch_messaging_enabled: state.options.project_branch_messaging_enabled,
            };
            let context_checkpoint =
                state
                    .context_checkpoint
                    .clone()
                    .map(|summary| DurableRunContextCheckpoint {
                        session_id: state.session_id,
                        summary,
                    });
            let activities = handle
                .activity_deltas
                .lock()
                .map_err(|_| {
                    LoomError::new(
                        ErrorCode::Internal,
                        "activity delta lock was poisoned",
                        true,
                    )
                })?
                .ordered_values();
            let cursor = handle.message_checkpoint.lock().map_err(|_| {
                LoomError::new(
                    ErrorCode::Internal,
                    "message checkpoint cursor lock was poisoned",
                    true,
                )
            })?;
            let reset_messages = cursor.attempt_id != state.run.attempt_id
                || state.messages.len() < cursor.message_count;
            let start_ordinal = if reset_messages {
                0
            } else {
                cursor.message_count.saturating_sub(1)
            };
            let start_index = start_ordinal.min(state.messages.len());
            let message_delta = DurableRunMessageDelta {
                start_ordinal: start_ordinal as u64,
                reset: reset_messages,
                messages: durable_run_messages_from_runtime(
                    &state.messages[start_index..],
                    &state.message_timeline_ordinals[start_index..],
                )?,
            };
            let project_manager_wait = state
                .pending_project_join
                .as_ref()
                .map(|continuation| {
                    let arguments = serde_json::from_value::<WaitForProjectChildrenArguments>(
                        continuation.call.arguments.clone(),
                    )
                    .map_err(|error| {
                        LoomError::new(
                            ErrorCode::MalformedPayload,
                            format!("parked project join has invalid child task IDs: {error}"),
                            false,
                        )
                    })?;
                    let timestamp = Timestamp::now();
                    Ok(loom_core::ProjectManagerWaitRecord {
                        wait_id: loom_core::ProjectManagerWaitId::from_uuid(
                            *continuation.call.id.as_uuid(),
                        ),
                        run_id: state.run.id,
                        attempt_id: state.run.attempt_id,
                        tool_call_id: continuation.call.id,
                        manager_session_id: state.session_id,
                        child_task_ids: arguments.task_ids,
                        status: loom_core::ProjectManagerWaitStatus::Waiting,
                        result_summary: None,
                        created_at: timestamp,
                        updated_at: timestamp,
                    })
                })
                .transpose()?;
            (
                state.session_id,
                summary,
                runtime_config,
                context_checkpoint,
                state.plan.clone(),
                message_delta,
                activities,
                project_manager_wait,
            )
        };

        let (filesystem_record, filesystem_ack) = {
            let filesystem = self.session_filesystems()?.get(&session_id).cloned();
            if let Some(filesystem) = filesystem {
                if let Some(versioned) = filesystem.export_delta_if_dirty()? {
                    let workspace_delta = versioned.delta;
                    let filesystem_state = versioned.state;
                    let checkpoints = workspace_delta.checkpoints;
                    let edits = workspace_delta
                        .edits
                        .into_iter()
                        .map(|edit| DurableFilesystemEdit {
                            id: edit.id,
                            path: edit.path,
                            before: edit.before,
                            before_bytes: edit.before_bytes,
                            after_revision: edit.after_revision,
                            source: edit.source,
                        })
                        .collect();
                    let changes = workspace_delta.changes;
                    let deleted_checkpoints = workspace_delta.deleted_checkpoints;
                    let deleted_edits = workspace_delta.deleted_edits;
                    let deleted_changes = workspace_delta.deleted_changes;
                    let directories = filesystem
                        .mounted_directories()?
                        .into_iter()
                        .map(|(path, source)| SessionDirectory {
                            path,
                            source: source.display().to_string(),
                        })
                        .collect::<Vec<_>>();
                    let repositories = self
                        .session_repositories()?
                        .get(&session_id)
                        .cloned()
                        .unwrap_or_default();
                    let persisted = PersistedSessionFilesystem {
                        filesystem: filesystem_state,
                        repositories: repositories.clone(),
                        directories: directories.clone(),
                    };
                    (
                        Some(DurableFilesystemRecord {
                            session_id,
                            root: persisted.filesystem.root.clone(),
                            control: persisted.filesystem.control,
                            checkpoints,
                            edits,
                            changes,
                            repositories,
                            directories,
                            payload: json_value(persisted)?,
                            delta: Some(DurableFilesystemDelta {
                                deleted_checkpoints,
                                deleted_edits,
                                deleted_changes,
                            }),
                        }),
                        Some((filesystem, versioned.generation)),
                    )
                } else {
                    (None, None)
                }
            } else {
                (None, None)
            }
        };

        let mut journal = self.journal()?;
        let (feed, captured_event_sequences) = journal.capture_session_feed(session_id);
        // Full retention ranking scans the retained feed, so amortize it until
        // at least 64 new global event sequences or 4 MiB of pending payloads
        // have arrived. The byte threshold prevents large event bodies from
        // overshooting the total feed retention budget between pruning passes.
        let sequence = feed.next_sequence.value();
        let last_pruned = self.last_feed_pruned_sequence.load(Ordering::Relaxed);
        let pending_feed_bytes = serde_json::to_vec(&(&feed.events, &feed.workspace_events))
            .map_err(|error| {
                LoomError::new(
                    ErrorCode::Persistence,
                    format!("could not size pending event feed: {error}"),
                    false,
                )
            })?
            .len();
        let prior_feed_bytes = self.feed_bytes_since_prune.load(Ordering::Relaxed);
        let accumulated_feed_bytes = prior_feed_bytes.saturating_add(pending_feed_bytes);
        let prune_feed =
            should_prune_worker_feed(sequence, last_pruned, prior_feed_bytes, pending_feed_bytes);
        let session = self.sessions()?.get(session_id)?;
        let session_next_sequence = self.sessions()?.next_sequence();
        let checkpoint = DurableRunCheckpointWrite {
            session: &session,
            session_next_sequence,
            prune_feed,
            summary: &summary,
            runtime_config: &runtime_config,
            context_checkpoint: context_checkpoint.as_ref(),
            plan: &plan,
            messages: &[],
            message_delta: Some(&message_delta),
            activities: &[],
            activity_deltas: Some(&activities),
            filesystem: filesystem_record.as_ref(),
            feed: &feed,
        };
        let checkpoint_result = match project_manager_wait.as_ref() {
            Some(wait) => {
                persistence.save_run_checkpoint_with_project_manager_wait(checkpoint, wait)
            }
            None => persistence.save_run_checkpoint(checkpoint),
        };
        if let Err(error) = checkpoint_result {
            self.persistence_failed.store(true, Ordering::SeqCst);
            return Err(error);
        }
        if let Some((filesystem, generation)) = filesystem_ack {
            filesystem.acknowledge_persisted_generation(generation)?;
        }
        if prune_feed {
            self.last_feed_pruned_sequence
                .store(sequence, Ordering::Relaxed);
            self.feed_bytes_since_prune.store(0, Ordering::Relaxed);
        } else {
            self.feed_bytes_since_prune
                .store(accumulated_feed_bytes, Ordering::Relaxed);
        }
        {
            let mut cursor = handle
                .message_checkpoint
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            cursor.attempt_id = summary.snapshot.attempt_id;
            cursor.message_count = usize::try_from(message_delta.start_ordinal)
                .unwrap_or(usize::MAX)
                .saturating_add(message_delta.messages.len());
        }
        if !activities.is_empty() {
            let mut pending = handle
                .activity_deltas
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            for activity in &activities {
                if pending.by_id.get(&activity.id) == Some(activity) {
                    pending.by_id.remove(&activity.id);
                }
            }
            let still_pending = pending.by_id.keys().copied().collect::<BTreeSet<_>>();
            pending
                .appended_order
                .retain(|activity_id| still_pending.contains(activity_id));
        }
        journal.acknowledge_session_feed(&captured_event_sequences);
        Ok(())
    }

    pub(crate) fn persist_state_with_recovery_updates(
        &self,
        recovery_updates: &BTreeMap<loom_core::RunId, DurableRunSummary>,
    ) -> Result<()> {
        self.latch_on_persistence_error(self.persist_state_inner(recovery_updates, None))
    }

    pub(crate) fn latch_on_persistence_error<T>(&self, result: Result<T>) -> Result<T> {
        if result.is_err() {
            self.persistence_failed.store(true, Ordering::SeqCst);
        }
        result
    }

    pub(crate) fn persist_state_inner(
        &self,
        recovery_updates: &BTreeMap<loom_core::RunId, DurableRunSummary>,
        idempotency_candidate: Option<(loom_core::RequestId, IdempotencyRecord)>,
    ) -> Result<()> {
        let Some(persistence) = &self.persistence else {
            return Ok(());
        };
        let _state_persist_guard = self.state_persist_gate.lock().map_err(|_| {
            LoomError::new(
                ErrorCode::Internal,
                "state persistence lock was poisoned",
                true,
            )
        })?;
        let handles = self.runs()?.values().cloned().collect::<Vec<_>>();
        for handle in handles {
            handle.flush_message_fragments(persistence)?;
        }
        let mut runs: BTreeMap<loom_core::RunId, AgentRuntimeState> = self
            .runs()?
            .iter()
            .map(|(run_id, handle)| (*run_id, handle.state()))
            .collect();
        let durable_run_plans = runs
            .iter()
            .map(|(run_id, state)| (*run_id, state.plan.clone()))
            .collect::<BTreeMap<_, _>>();
        let mut durable_run_summaries: BTreeMap<loom_core::RunId, DurableRunSummary> = self
            .persisted_runs()?
            .iter()
            .map(|(run_id, summary)| {
                (
                    *run_id,
                    DurableRunSummary {
                        snapshot: summary.snapshot.clone(),
                        usage: summary.usage.clone(),
                        attempts: None,
                        execution_state: None,
                        interactions: None,
                    },
                )
            })
            .collect();
        for (run_id, state) in &runs {
            durable_run_summaries.insert(
                *run_id,
                DurableRunSummary {
                    snapshot: state.run.clone(),
                    usage: state.usage.clone(),
                    attempts: Some(state.attempts.clone()),
                    execution_state: Some(execution_state_from_runtime(state)?),
                    interactions: Some(state.interactions.clone()),
                },
            );
        }
        durable_run_summaries.extend(
            recovery_updates
                .iter()
                .map(|(run_id, summary)| (*run_id, summary.clone())),
        );
        let durable_run_context_checkpoints =
            runs.iter()
                .map(|(run_id, state)| {
                    (
                        *run_id,
                        state.context_checkpoint.clone().map(|summary| {
                            DurableRunContextCheckpoint {
                                session_id: state.session_id,
                                summary,
                            }
                        }),
                    )
                })
                .collect::<BTreeMap<_, _>>();
        let durable_run_runtime_configs = runs
            .iter()
            .map(|(run_id, state)| {
                let mut context_inspection = state.context_inspection.clone();
                if let Some(inspection) = &mut context_inspection {
                    inspection.summary = None;
                }
                Ok((
                    *run_id,
                    DurableRunRuntimeConfig {
                        system_instructions: state.task.system_instructions.clone(),
                        repository_instructions: state.task.repository_instructions.clone(),
                        approval_policy: state.approval_policy.clone(),
                        limits: state.options.limits.clone(),
                        context_options: state.options.context.clone(),
                        checkpoint_id: state.options.checkpoint_id,
                        input_cost_micros_per_1k: state.options.input_cost_micros_per_1k,
                        output_cost_micros_per_1k: state.options.output_cost_micros_per_1k,
                        context_inspection,
                        project_delegation_enabled: state.options.project_delegation_enabled,
                        project_messaging_enabled: state.options.project_messaging_enabled,
                        project_inspection_enabled: state.options.project_inspection_enabled,
                        project_child_control_enabled: state.options.project_child_control_enabled,
                        project_worktree_enabled: state.options.project_worktree_enabled,
                        project_review_enabled: state.options.project_review_enabled,
                        project_integration_enabled: state.options.project_integration_enabled,
                        project_branch_messaging_enabled: state
                            .options
                            .project_branch_messaging_enabled,
                    },
                ))
            })
            .collect::<Result<BTreeMap<_, _>>>()?;
        let mut durable_run_messages = BTreeMap::new();
        let mut durable_run_activities = BTreeMap::new();
        for (run_id, state) in &mut runs {
            durable_run_messages.insert(
                *run_id,
                durable_run_messages_from_runtime(
                    &state.messages,
                    &state.message_timeline_ordinals,
                )?,
            );
            durable_run_activities.insert(*run_id, std::mem::take(&mut state.activities));
        }
        let loaded_repositories = self.session_repositories()?.clone();
        let mut filesystem_records = Vec::new();
        let mut filesystem_generations = Vec::new();
        for (session_id, filesystem) in self.session_filesystems()?.iter() {
            let Some(versioned) = filesystem.export_delta_if_dirty()? else {
                continue;
            };
            let workspace_delta = versioned.delta;
            let filesystem_state = versioned.state;
            let checkpoints = workspace_delta.checkpoints;
            let edits = workspace_delta
                .edits
                .into_iter()
                .map(|edit| DurableFilesystemEdit {
                    id: edit.id,
                    path: edit.path,
                    before: edit.before,
                    before_bytes: edit.before_bytes,
                    after_revision: edit.after_revision,
                    source: edit.source,
                })
                .collect();
            let changes = workspace_delta.changes;
            let deleted_checkpoints = workspace_delta.deleted_checkpoints;
            let deleted_edits = workspace_delta.deleted_edits;
            let deleted_changes = workspace_delta.deleted_changes;
            let directories = filesystem
                .mounted_directories()?
                .into_iter()
                .map(|(path, source)| SessionDirectory {
                    path,
                    source: source.display().to_string(),
                })
                .collect::<Vec<_>>();
            let repositories = loaded_repositories
                .get(session_id)
                .cloned()
                .unwrap_or_default();
            let persisted = PersistedSessionFilesystem {
                filesystem: filesystem_state,
                repositories: repositories.clone(),
                directories: directories.clone(),
            };
            filesystem_records.push(DurableFilesystemRecord {
                session_id: *session_id,
                root: persisted.filesystem.root.clone(),
                control: persisted.filesystem.control,
                checkpoints,
                edits,
                changes,
                repositories,
                directories,
                payload: json_value(persisted)?,
                delta: Some(DurableFilesystemDelta {
                    deleted_checkpoints,
                    deleted_edits,
                    deleted_changes,
                }),
            });
            filesystem_generations.push((filesystem.clone(), versioned.generation));
        }
        let sessions = self.sessions()?.export_state();
        let mut journal = self.journal()?;
        let feed = DurableFeedState {
            next_sequence: journal.next_sequence,
            retention_limit: journal.retention_limit,
            events: journal.pending_events.clone(),
            workspace_events: journal.pending_workspace_events.clone(),
        };
        let session_settings = DurableSessionSettings {
            approval_policies: self.session_policies()?.clone(),
            auto_approve_actions: self.auto_approve_actions()?.clone(),
        };
        let workspace_configs = self.workspace_configs()?.clone();
        let provider_state = DurableProviderState {
            configs: self
                .providers
                .export_configs()?
                .into_iter()
                .map(|config| (config.id.clone(), config))
                .collect(),
            health: self.providers.export_health()?,
        };
        let workspace_records = self.workspace_records()?.export_state();
        let provider_usage = self.providers.usage()?;
        let idempotency = self
            .idempotency_store
            .durable_records(idempotency_candidate.as_ref())?;
        #[cfg(test)]
        if self.fail_next_state_save.swap(false, Ordering::SeqCst) {
            self.persistence_failed.store(true, Ordering::SeqCst);
            return Err(LoomError::new(
                ErrorCode::Internal,
                "injected durable state save failure",
                true,
            ));
        }
        let result = persistence.save_state(DurableStateWrite {
            sessions: &sessions,
            workspaces: Some(&workspace_records),
            settings: Some(&session_settings),
            workspace_configs: Some(&workspace_configs),
            providers: Some(&provider_state),
            usage: Some(&provider_usage),
            idempotency: Some(&idempotency),
            run_summaries: Some(&durable_run_summaries),
            run_runtime_configs: Some(&durable_run_runtime_configs),
            run_context_checkpoints: Some(&durable_run_context_checkpoints),
            run_plans: Some(&durable_run_plans),
            run_messages: Some(&durable_run_messages),
            run_activities: Some(&durable_run_activities),
            filesystem_records: Some(&filesystem_records),
            feed: Some(&feed),
        });
        if result.is_err() {
            self.persistence_failed.store(true, Ordering::SeqCst);
        }
        if result.is_ok() {
            for (filesystem, generation) in filesystem_generations {
                filesystem.acknowledge_persisted_generation(generation)?;
            }
            journal.pending_events.clear();
            journal.pending_workspace_events.clear();
            self.last_feed_pruned_sequence
                .store(feed.next_sequence.value(), Ordering::Relaxed);
            self.feed_bytes_since_prune.store(0, Ordering::Relaxed);
        }
        result
    }

    pub fn flush(&self) -> Result<()> {
        if *self.request_lifecycle.read().map_err(|_| {
            LoomError::new(
                ErrorCode::Internal,
                "backend request lifecycle lock was poisoned",
                true,
            )
        })? != 0
        {
            return Err(LoomError::conflict("backend is shutting down"));
        }
        self.persist_state()
    }

    /// Stops active run workers, persists their paused continuation state,
    /// joins all worker threads, and releases exclusive database ownership.
    /// Requests through existing connections are rejected after shutdown.
    pub fn shutdown(&self) -> Result<()> {
        let mut shutting_down = self.request_lifecycle.write().map_err(|_| {
            LoomError::new(
                ErrorCode::Internal,
                "backend request lifecycle lock was poisoned",
                true,
            )
        })?;
        if *shutting_down == 2 {
            return Ok(());
        }
        *shutting_down = 1;
        let handles = self
            .runs()?
            .values()
            .cloned()
            .collect::<Vec<Arc<RunHandle>>>();
        for handle in handles {
            if handle.is_running() {
                handle.control.request_pause();
                handle.wait_until_idle()?;
                if let Some(error) = handle.take_failure() {
                    return Err(error);
                }
                if handle.control.is_stopping() {
                    let state = handle.state().run.state;
                    // Only active runs need to be parked. Awaiting approval and
                    // waiting for input are already durable, resumable stops and
                    // must not be downgraded to Paused by shutdown.
                    if matches!(
                        state,
                        AgentRunState::Planning
                            | AgentRunState::Executing
                            | AgentRunState::Evaluating
                    ) {
                        let mut runtime = handle.try_runtime()?;
                        runtime.pause()?;
                        handle.refresh(&runtime);
                    }
                    handle.control.clear_request();
                }
            }
            handle.join_worker()?;
        }
        // A fail-stopped backend must not write more state, but shutdown still
        // has to release database ownership so a restart can reopen it.
        let persist_result = if self.persistence_failed.load(Ordering::SeqCst) {
            Ok(())
        } else {
            self.persist_state()
        };
        if let Some(persistence) = self.persistence.as_ref() {
            persistence.release_exclusive_writer()?;
        }
        *shutting_down = 2;
        persist_result
    }

    pub(crate) fn append_recovery_events(
        self: &Arc<Self>,
        session_id: AgentSessionId,
        events: Vec<AgentEvent>,
    ) -> Result<()> {
        for event in events {
            let state = session_state_for_event(&event);
            self.journal()?.append_agent(session_id, event);
            if let Some(state) = state {
                let current = self.sessions()?.get(session_id)?.state;
                if current != state {
                    let (_, record) = self.sessions()?.transition(session_id, state)?;
                    self.journal()?.append_session(record);
                }
            }
        }
        Ok(())
    }
}
