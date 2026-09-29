use super::*;

impl InProcessBackend {
    pub(crate) fn restore_session_filesystem(
        &self,
        session_id: AgentSessionId,
    ) -> Result<Workspace> {
        if let Some(filesystem) = self.session_filesystems()?.get(&session_id).cloned() {
            return Ok(filesystem);
        }
        let _restore_guard = self.session_filesystem_service.restore_guard()?;
        if let Some(filesystem) = self.session_filesystems()?.get(&session_id).cloned() {
            return Ok(filesystem);
        }
        if !self.persisted_session_filesystems()?.contains(&session_id) {
            return Err(LoomError::new(
                ErrorCode::RecoveryRequired,
                format!("filesystem for session {session_id} is unavailable"),
                true,
            ));
        }
        let durable = self
            .persistence
            .as_ref()
            .ok_or_else(|| {
                LoomError::new(
                    ErrorCode::RecoveryRequired,
                    format!("filesystem for session {session_id} has no persistence store"),
                    true,
                )
            })?
            .load_filesystem_record(session_id)?
            .ok_or_else(|| {
                LoomError::new(
                    ErrorCode::MalformedPayload,
                    format!("persisted filesystem record for session {session_id} is missing"),
                    false,
                )
            })?;
        let mut payload = durable.payload;
        let repositories = durable.repositories;
        let directories = durable.directories;
        payload["filesystem"]["checkpoints"] = json_value(durable.checkpoints)?;
        let edits = durable.edits;
        let changes = durable.changes;
        let mut persisted: PersistedSessionFilesystem = from_json(payload)?;
        persisted.filesystem.edits = edits
            .into_iter()
            .map(|edit| loom_workspace::WorkspaceEditHistory {
                id: edit.id,
                path: edit.path,
                before: edit.before,
                before_bytes: edit.before_bytes,
                after_revision: edit.after_revision,
                source: edit.source,
            })
            .collect();
        persisted.filesystem.changes = changes;
        persisted.repositories = repositories;
        persisted.directories = directories;
        if durable.session_id != session_id
            || persisted.filesystem.session_id != session_id
            || persisted.filesystem.root != durable.root
            || persisted.filesystem.control != durable.control
        {
            return Err(LoomError::new(
                ErrorCode::MalformedPayload,
                format!("persisted filesystem identity for session {session_id} is invalid"),
                false,
            ));
        }
        let session = self.sessions()?.get(session_id)?;
        let expected_root = self
            .session_root_base
            .join(session.workspace_id.to_string())
            .join(session_id.to_string())
            .join("fs");
        let canonical_expected_root = fs::canonicalize(&expected_root).map_err(|error| {
            LoomError::new(
                ErrorCode::RecoveryRequired,
                format!("session filesystem root for {session_id} is unavailable: {error}"),
                true,
            )
        })?;
        if Path::new(&persisted.filesystem.root) != canonical_expected_root {
            return Err(LoomError::new(
                ErrorCode::MalformedPayload,
                format!("persisted filesystem root for session {session_id} is invalid"),
                false,
            ));
        }
        let filesystem = Workspace::open_for_restore(session_id, &canonical_expected_root)?;
        for directory in &persisted.directories {
            filesystem.mount_directory(&directory.path, &directory.source)?;
        }
        filesystem.restore_state(persisted.filesystem)?;
        let owned_worktree_repository_ids = self
            .persistence
            .as_ref()
            .map(|persistence| persistence.load_project_snapshot_for_session(session_id))
            .transpose()?
            .flatten()
            .map(|project| {
                project
                    .worktrees
                    .into_iter()
                    .filter(|worktree| worktree.child_session_id == session_id)
                    .map(|worktree| worktree.child_repository_id)
                    .collect::<BTreeSet<_>>()
            })
            .unwrap_or_default();
        let mut missing_worktree_repositories = Vec::new();
        for (repository_id, repository) in &persisted.repositories {
            if *repository_id != repository.id {
                return Err(LoomError::new(
                    ErrorCode::MalformedPayload,
                    format!("repository key does not match repository {}", repository.id),
                    false,
                ));
            }
            let path = filesystem.resolve_path(&repository.path, true)?;
            if !path.exists() && owned_worktree_repository_ids.contains(repository_id) {
                // The worktree record is authoritative for recovery. A missing
                // checkout must not prevent the child session from opening;
                // the scheduler or cleanup request will reconcile its durable
                // worktree state before using the repository.
                missing_worktree_repositories.push(*repository_id);
                continue;
            }
            let service = GitService::open(path)?;
            self.session_vcs()?
                .insert((session_id, *repository_id), service);
        }
        for repository_id in &missing_worktree_repositories {
            persisted.repositories.remove(repository_id);
        }
        self.session_repositories()?
            .insert(session_id, persisted.repositories);
        if !missing_worktree_repositories.is_empty() {
            filesystem.mark_state_dirty()?;
        }
        self.session_filesystems()?
            .insert(session_id, filesystem.clone());
        self.persisted_session_filesystems()?.remove(&session_id);
        Ok(filesystem)
    }

    pub(crate) fn persisted_filesystem_record(
        &self,
        session_id: AgentSessionId,
    ) -> Result<Option<PersistedSessionFilesystem>> {
        if !self.persisted_session_filesystems()?.contains(&session_id) {
            return Ok(None);
        }
        let Some(record) = self
            .persistence
            .as_ref()
            .ok_or_else(|| {
                LoomError::new(
                    ErrorCode::RecoveryRequired,
                    format!("filesystem for session {session_id} has no persistence store"),
                    true,
                )
            })?
            .load_filesystem_record(session_id)?
        else {
            return Ok(None);
        };
        let mut payload = record.payload;
        let repositories = record.repositories;
        let directories = record.directories;
        payload["filesystem"]["checkpoints"] = json_value(record.checkpoints)?;
        let edits = record.edits;
        let changes = record.changes;
        let mut persisted: PersistedSessionFilesystem = from_json(payload)?;
        persisted.filesystem.edits = edits
            .into_iter()
            .map(|edit| loom_workspace::WorkspaceEditHistory {
                id: edit.id,
                path: edit.path,
                before: edit.before,
                before_bytes: edit.before_bytes,
                after_revision: edit.after_revision,
                source: edit.source,
            })
            .collect();
        persisted.filesystem.changes = changes;
        persisted.repositories = repositories;
        persisted.directories = directories;
        if record.session_id != session_id
            || persisted.filesystem.session_id != session_id
            || persisted.filesystem.root != record.root
            || persisted.filesystem.control != record.control
        {
            return Err(LoomError::new(
                ErrorCode::MalformedPayload,
                format!("persisted filesystem identity for session {session_id} is invalid"),
                false,
            ));
        }
        Ok(Some(persisted))
    }

    pub(crate) fn create_session_filesystem(
        &self,
        workspace_id: WorkspaceId,
        session_id: AgentSessionId,
    ) -> Result<Workspace> {
        let root = self
            .session_root_base
            .join(workspace_id.to_string())
            .join(session_id.to_string())
            .join("fs");
        fs::create_dir_all(&root).map_err(|error| {
            LoomError::new(
                ErrorCode::WorkspaceAccessDenied,
                format!("could not create session filesystem root: {error}"),
                false,
            )
        })?;
        Workspace::open(session_id, root)
    }

    pub(crate) fn restore_persisted(self: &Arc<Self>) -> Result<()> {
        let Some(persistence) = self.persistence.clone() else {
            return Ok(());
        };
        persistence.prune_expired_idempotency_records(Timestamp::from_unix_millis(
            current_unix_millis(),
        ))?;
        let startup_started = Instant::now();
        let mut needs_persist = false;
        let Some(sessions) = persistence.load_sessions()? else {
            let mut provider_configs = persistence.load_provider_configs()?;
            let migrated = self
                .providers
                .migrate_api_key_credentials(&mut provider_configs)?;
            self.providers.restore_configs(provider_configs)?;
            self.providers
                .restore_health(persistence.load_provider_health()?)?;
            self.providers
                .restore_usage(persistence.load_provider_usage()?)?;
            *self.workspace_configs()? = persistence.load_workspace_configs()?;
            if migrated {
                self.persist_state()?;
            }
            return Ok(());
        };
        let session_settings = persistence.load_session_settings()?;
        let state = PersistedBackendState {
            sessions,
            workspace_records: persistence.load_workspaces()?.unwrap_or_default(),
            journal: persistence
                .load_feed_header()?
                .map(|feed| EventJournal {
                    next_sequence: feed.next_sequence,
                    events: Vec::new(),
                    pending_events: Vec::new(),
                    workspace_events: Vec::new(),
                    pending_workspace_events: Vec::new(),
                    retention_limit: feed.retention_limit,
                })
                .unwrap_or_default(),
            session_policies: session_settings.approval_policies,
            auto_approve_actions: session_settings.auto_approve_actions,
            provider_configs: persistence.load_provider_configs()?,
            provider_health: persistence.load_provider_health()?,
            workspace_configs: persistence.load_workspace_configs()?,
            provider_usage: persistence.load_provider_usage()?,
            idempotency: persistence
                .load_idempotency_records()?
                .into_iter()
                .map(|(id, record)| {
                    Ok((
                        id,
                        IdempotencyRecord {
                            created_at: record.created_at,
                            expires_at: record.expires_at,
                            request: from_json(record.request)?,
                            response: from_json(record.response)?,
                        },
                    ))
                })
                .collect::<Result<BTreeMap<_, _>>>()?,
        };
        log::info!(
            "loaded persisted catalogs and feed cursors in {} ms",
            startup_started.elapsed().as_millis()
        );
        let sessions = SessionManager::from_state(state.sessions)?;
        {
            let mut target = self.sessions()?;
            *target = sessions;
        }
        {
            let mut target = self.journal()?;
            if state
                .journal
                .events
                .windows(2)
                .any(|events| events[0].sequence >= events[1].sequence)
                || state
                    .journal
                    .events
                    .last()
                    .is_some_and(|event| event.sequence != state.journal.next_sequence)
            {
                return Err(LoomError::new(
                    ErrorCode::MalformedPayload,
                    "persisted event journal sequences are invalid",
                    false,
                ));
            }
            *target = state.journal;
        }
        self.idempotency_store.replace_records(state.idempotency)?;
        {
            let mut target = self.session_policies()?;
            *target = state.session_policies;
        }
        {
            let mut target = self.auto_approve_actions()?;
            *target = state.auto_approve_actions;
        }
        let mut provider_configs = state.provider_configs;
        if self
            .providers
            .migrate_api_key_credentials(&mut provider_configs)?
        {
            needs_persist = true;
        }
        self.providers.restore_configs(provider_configs)?;
        self.providers.restore_health(state.provider_health)?;
        self.providers.restore_usage(state.provider_usage)?;
        *self.workspace_configs()? = state.workspace_configs;

        *self.workspace_records()? =
            loom_session::WorkspaceManager::from_state(state.workspace_records)?;

        for session_id in persistence.list_filesystem_sessions()? {
            self.sessions()?.get(session_id)?;
            self.persisted_session_filesystems()?.insert(session_id);
        }

        let active_run_summaries = persistence.load_active_run_summaries()?;
        let active_run_count = active_run_summaries.len();
        let mut lazy_run_count = 0;
        let mut recovery_updates = BTreeMap::new();
        let mut run_summaries = BTreeMap::new();
        let mut restored_runs = BTreeMap::new();
        for (run_id, mut summary) in active_run_summaries {
            let mut snapshot = summary.snapshot.clone();
            let run_state = snapshot.state;
            let usage = summary.usage.clone();
            self.sessions()?.get(snapshot.session_id)?;
            run_summaries.insert(
                run_id,
                PersistedRunSummary {
                    snapshot: snapshot.clone(),
                    usage: usage.clone(),
                },
            );
            let mut execution_state = persistence.load_run_execution_state(run_id)?;
            let safely_deferred = run_can_be_deferred_during_restore(
                run_state,
                execution_state
                    .as_ref()
                    .map(|state| state.pending_tool_execution.is_some()),
                execution_state
                    .as_ref()
                    .map(|state| state.pending_project_join.is_some()),
            );
            if safely_deferred {
                if matches!(
                    run_state,
                    AgentRunState::Planning | AgentRunState::Executing | AgentRunState::Evaluating
                ) {
                    let updated_at = Timestamp::now();
                    snapshot.state = AgentRunState::Paused;
                    snapshot.updated_at = updated_at;
                    snapshot.completed_at = None;
                    let execution = execution_state.as_mut().ok_or_else(|| {
                        LoomError::new(
                            ErrorCode::RecoveryRequired,
                            format!("persisted run {run_id} has no typed execution state"),
                            true,
                        )
                    })?;
                    execution.state = AgentRunState::Paused;
                    let mut attempts = persistence.load_run_attempts(run_id)?;
                    let attempt = attempts
                        .iter_mut()
                        .find(|attempt| attempt.id == snapshot.attempt_id)
                        .ok_or_else(|| {
                            LoomError::new(
                                ErrorCode::RecoveryRequired,
                                format!("persisted run {run_id} has no current attempt"),
                                true,
                            )
                        })?;
                    attempt.state = AgentRunState::Paused;
                    attempt.completed_at = None;
                    summary.snapshot = snapshot.clone();
                    summary.attempts = Some(attempts);
                    summary.execution_state = execution_state;
                    summary.interactions = None;
                    recovery_updates.insert(run_id, summary);
                    run_summaries.insert(
                        run_id,
                        PersistedRunSummary {
                            snapshot: snapshot.clone(),
                            usage: usage.clone(),
                        },
                    );
                    self.append_recovery_events(
                        snapshot.session_id,
                        vec![AgentEvent::RunStateChanged {
                            run_id,
                            state: AgentRunState::Paused,
                        }],
                    )?;
                }
                lazy_run_count += 1;
                continue;
            }
            let runtime_config = persistence
                .load_run_runtime_config(run_id)?
                .ok_or_else(|| {
                    LoomError::new(
                        ErrorCode::RecoveryRequired,
                        format!("persisted run {run_id} has no runtime configuration"),
                        true,
                    )
                })?;
            let mut runtime_state = runtime_state_from_durable_config(
                run_summaries.get(&run_id).ok_or_else(|| {
                    LoomError::new(
                        ErrorCode::RecoveryRequired,
                        format!("persisted run {run_id} summary is unavailable"),
                        true,
                    )
                })?,
                runtime_config,
            )?;
            let execution_state = execution_state.take().ok_or_else(|| {
                LoomError::new(
                    ErrorCode::RecoveryRequired,
                    format!("persisted run {run_id} has no typed execution state"),
                    true,
                )
            })?;
            hydrate_runtime_execution_state(&mut runtime_state, execution_state)?;
            runtime_state.plan = persistence.load_run_plan(run_id)?;
            let durable_messages = persistence.load_run_messages(run_id)?;
            runtime_state.message_timeline_ordinals = durable_messages
                .iter()
                .map(|message| message.timeline_ordinal)
                .collect();
            runtime_state.messages = persisted_run_messages(durable_messages);
            hydrate_run_context_checkpoint(&persistence, run_id, &mut runtime_state)?;
            runtime_state.activities = persistence.load_run_activities(run_id)?;
            runtime_state.attempts = persistence.load_run_attempts(run_id)?;
            if runtime_state.attempts.is_empty() {
                return Err(LoomError::new(
                    ErrorCode::RecoveryRequired,
                    format!("persisted run {run_id} has no typed attempt history"),
                    true,
                ));
            }
            runtime_state.interactions = persistence.load_run_interactions(run_id)?;
            if runtime_state.run.id != run_id {
                return Err(LoomError::new(
                    ErrorCode::MalformedPayload,
                    format!("persisted run key does not match runtime state {run_id}"),
                    false,
                ));
            }
            let session = self.sessions()?.get(runtime_state.session_id)?;
            let workspace = self.restore_session_filesystem(session.id)?;
            let mut recovery_reason = None;
            let provider =
                match self.provider_at(&runtime_state.task.model, runtime_state.provider_cursor) {
                    Ok(provider) => provider,
                    Err(error) => {
                        let descriptor = self
                            .providers
                            .describe_model(&runtime_state.task.model)
                            .unwrap_or_else(|_| ModelDescriptor {
                                id: runtime_state.task.model.clone(),
                                provider: ProviderId::new("recovered"),
                                display_name: "Unavailable persisted model".to_owned(),
                                context_window: None,
                                max_input_tokens: None,
                                max_output_tokens: None,
                                capabilities: ModelCapabilities::default(),
                            });
                        recovery_reason = Some(error.message.clone());
                        Box::new(UnavailableProvider::new(descriptor, error))
                    }
                };
            let tools = ToolExecutor::new_with_workspace(workspace)
                .with_github_token(self.providers.github_account_token().ok())
                .with_github_write_access(self.providers.github_write_access());
            let tools = self.with_project_agent_tools(
                tools,
                runtime_state.session_id,
                runtime_state.task.model.clone(),
                ProjectAgentToolGrants {
                    delegation: runtime_state.options.project_delegation_enabled,
                    messaging: runtime_state.options.project_messaging_enabled,
                    branch_messaging: runtime_state.options.project_branch_messaging_enabled,
                    inspection: runtime_state.options.project_inspection_enabled,
                    child_control: runtime_state.options.project_child_control_enabled,
                    worktree: runtime_state.options.project_worktree_enabled,
                    review: runtime_state.options.project_review_enabled,
                    integration: runtime_state.options.project_integration_enabled,
                },
            )?;
            let mut runtime = AgentRuntime::from_state(runtime_state, provider, tools)?;
            if runtime.session_id() != session.id {
                return Err(LoomError::new(
                    ErrorCode::MalformedPayload,
                    format!("persisted run {run_id} references a different session"),
                    false,
                ));
            }
            let mut recovery_events = runtime.recover_after_restart()?;
            if let Some(reason) = recovery_reason {
                recovery_events.push(AgentEvent::RecoveryRequired { run_id, reason });
            }
            let handle = self.register_runtime(runtime);
            if !recovery_events.is_empty()
                && let Some(summary) = run_summaries.get_mut(&run_id)
            {
                summary.snapshot = handle.state().run;
            }
            restored_runs.insert(run_id, handle);
            if !recovery_events.is_empty() {
                self.append_recovery_events(session.id, recovery_events)?;
                needs_persist = true;
            }
        }
        let restored_run_count = restored_runs.len();
        *self.persisted_runs()? = run_summaries;
        *self.runs()? = restored_runs;
        if needs_persist {
            self.persist_state_with_recovery_updates(&recovery_updates)?;
        } else if !recovery_updates.is_empty() {
            let mut journal = self.journal()?;
            let feed = DurableFeedState {
                next_sequence: journal.next_sequence,
                retention_limit: journal.retention_limit,
                events: journal.pending_events.clone(),
                workspace_events: journal.pending_workspace_events.clone(),
            };
            persistence.save_recovery_updates(&recovery_updates, &feed)?;
            journal.pending_events.clear();
        }
        // Finish durable cascade intents before the scheduler can reconcile or
        // admit any queued project work after restart.
        self.connect()
            .recover_pending_project_cancellation_cascades()?;
        self.reconcile_project_tasks_and_resume_queued(true)?;
        let lazy_filesystem_count = self.persisted_session_filesystems()?.len();
        log::info!(
            "indexed {} resumable runs, restored {} runtimes, deferred {} runtimes, and left {} filesystem services lazy in {} ms total",
            active_run_count,
            restored_run_count,
            lazy_run_count,
            lazy_filesystem_count,
            startup_started.elapsed().as_millis()
        );
        Ok(())
    }
}
