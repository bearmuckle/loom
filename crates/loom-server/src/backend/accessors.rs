use super::*;

impl InProcessBackend {
    pub fn connect(self: &Arc<Self>) -> InProcessConnection {
        InProcessConnection {
            backend: Arc::clone(self),
            negotiated_capabilities: Arc::new(Mutex::new(None)),
            identity: Arc::new(Mutex::new(ConnectionIdentity::default())),
            pre_login: false,
            max_response_bytes: None,
        }
    }

    pub fn connect_authenticated(self: &Arc<Self>, auth: AuthSession) -> InProcessConnection {
        let participant = auth.participant().cloned();
        let presence = participant
            .as_ref()
            .map(|participant| Arc::new(self.access.connect(participant)));
        InProcessConnection {
            backend: Arc::clone(self),
            negotiated_capabilities: Arc::new(Mutex::new(None)),
            identity: Arc::new(Mutex::new(ConnectionIdentity {
                auth: Some(auth),
                participant,
                presence,
            })),
            pre_login: false,
            max_response_bytes: None,
        }
    }

    /// A connection accepted without any credential, because GitHub sign-in is
    /// enabled on this worker.
    ///
    /// It may negotiate, discover capabilities, read worker status and the
    /// current participant, and sign in or out; every other request is refused
    /// with [`ErrorCode::AuthenticationRequired`] until a person signs in on it.
    pub fn connect_pre_login(self: &Arc<Self>) -> InProcessConnection {
        InProcessConnection {
            backend: Arc::clone(self),
            negotiated_capabilities: Arc::new(Mutex::new(None)),
            identity: Arc::new(Mutex::new(ConnectionIdentity::default())),
            pre_login: true,
            max_response_bytes: None,
        }
    }

    /// Whether this worker accepts GitHub sign-ins.
    pub fn github_sign_in_enabled(&self) -> bool {
        self.identity.is_enabled()
    }

    /// The policy a connection's sign-in follows.
    pub fn github_sign_in_config(&self) -> GitHubSignInConfig {
        self.identity.config()
    }

    /// Replaces the worker's GitHub sign-in policy.
    ///
    /// A listener sets the policy it was started with here, and an in-process
    /// caller can do the same before it connects. The default policy enables
    /// sign-in for any GitHub account; [`GitHubSignInConfig::disabled`] turns it
    /// off, and an allowlist narrows who may sign in.
    pub fn set_github_sign_in_config(&self, config: GitHubSignInConfig) {
        self.identity.configure(config);
    }

    /// Adds people to the participant directory, so an owner can grant them
    /// access before their first connection. The directory is saved with the
    /// durable state.
    pub fn register_participants(
        &self,
        participants: &[loom_core::ParticipantIdentity],
    ) -> Result<()> {
        for participant in participants {
            self.access.register_participant(participant);
        }
        self.persist_state()
    }

    pub fn provider_registry(&self) -> ProviderRegistry {
        self.providers.clone()
    }

    pub(crate) fn provider(&self, model: &ModelId) -> Result<Box<dyn ModelProvider>> {
        self.providers.create_provider(model)
    }

    pub(crate) fn provider_at(
        &self,
        model: &ModelId,
        cursor: usize,
    ) -> Result<Box<dyn ModelProvider>> {
        self.providers.create_provider_at(model, cursor)
    }

    /// The credential pin a new run of `session_id` with `model` gets, and
    /// the provider authenticated with it.
    ///
    /// A session with a selected credential pins that credential's latest
    /// version. A selected credential that is missing, revoked, or for a
    /// different provider is an authentication error; it never falls back to
    /// another account. A session without a selection uses the worker-wide
    /// provider configuration.
    pub(crate) fn provider_for_new_run(
        &self,
        session_id: AgentSessionId,
        model: &ModelId,
    ) -> Result<(loom_core::RunCredentialPin, Box<dyn ModelProvider>)> {
        let provider_id = self.providers.provider_id_for_model(model)?;
        let Some((selection, credential)) = self.provider_credentials.selection(session_id) else {
            let pin = loom_core::RunCredentialPin {
                credential_id: None,
                version: None,
                provider: provider_id.as_str().to_owned(),
                model: model.as_str().to_owned(),
                owner: None,
                owner_label: "worker".to_owned(),
            };
            return Ok((pin, self.provider(model)?));
        };
        let credential = credential.ok_or_else(|| {
            services::provider_credentials::credential_error(format!(
                "the session's selected credential {} no longer exists",
                selection.credential_id
            ))
        })?;
        if credential.provider != provider_id.as_str() {
            return Err(services::provider_credentials::credential_error(format!(
                "the session's selected credential is for provider '{}', not '{}'",
                credential.provider,
                provider_id.as_str()
            )));
        }
        let pin = loom_core::RunCredentialPin {
            credential_id: Some(credential.id),
            version: Some(credential.version),
            provider: credential.provider.clone(),
            model: model.as_str().to_owned(),
            owner: credential.owner,
            owner_label: credential.owner_label.clone(),
        };
        let provider = self.pinned_provider(&pin, model, 0)?;
        Ok((pin, provider))
    }

    /// The provider for a run, authenticated with the credential version the
    /// run pinned. Runs without a pinned credential use the worker-wide
    /// provider configuration.
    pub(crate) fn provider_for_run(
        &self,
        run_id: loom_core::RunId,
        model: &ModelId,
        cursor: usize,
    ) -> Result<Box<dyn ModelProvider>> {
        match self.provider_credentials.run_pin(run_id) {
            Some(pin) if pin.credential_id.is_some() => self.pinned_provider(&pin, model, cursor),
            _ => self.provider_at(model, cursor),
        }
    }

    fn pinned_provider(
        &self,
        pin: &loom_core::RunCredentialPin,
        model: &ModelId,
        cursor: usize,
    ) -> Result<Box<dyn ModelProvider>> {
        let (Some(credential_id), Some(version)) = (pin.credential_id, pin.version) else {
            return self.provider_at(model, cursor);
        };
        let secret = self.provider_credentials.resolve_secret(
            self.providers.instance_credentials()?.as_ref(),
            credential_id,
            version,
        )?;
        let inner = self
            .providers
            .create_provider_with_secret(model, cursor, Some(secret))?;
        Ok(Box::new(PinnedCredentialProvider {
            inner,
            credentials: Arc::clone(&self.provider_credentials),
            credential_id,
        }))
    }

    /// Records the pin of a run that just started.
    pub(crate) fn pin_run_credential(
        &self,
        run_id: loom_core::RunId,
        pin: loom_core::RunCredentialPin,
    ) -> Result<()> {
        self.provider_credentials
            .pin(self.providers.instance_credentials()?.as_ref(), run_id, pin)
    }

    pub(crate) fn sessions(&self) -> Result<MutexGuard<'_, SessionManager>> {
        self.session_service.sessions()
    }

    pub(crate) fn workspace_records(
        &self,
    ) -> Result<MutexGuard<'_, loom_session::WorkspaceManager>> {
        self.workspace_service.records()
    }

    pub(crate) fn session_filesystems(
        &self,
    ) -> Result<MutexGuard<'_, BTreeMap<AgentSessionId, Workspace>>> {
        self.session_filesystem_service.filesystems()
    }

    pub(crate) fn persisted_session_filesystems(
        &self,
    ) -> Result<MutexGuard<'_, BTreeSet<AgentSessionId>>> {
        self.session_filesystem_service.persisted()
    }

    pub(crate) fn session_repositories(
        &self,
    ) -> Result<MutexGuard<'_, BTreeMap<AgentSessionId, BTreeMap<RepositoryId, SessionRepository>>>>
    {
        self.repository_service.records()
    }

    pub(crate) fn session_vcs(
        &self,
    ) -> Result<MutexGuard<'_, BTreeMap<(AgentSessionId, RepositoryId), GitService>>> {
        self.repository_service.vcs()
    }

    /// Repositories this worker node has already cloned and cached.
    pub(crate) fn cached_repositories(&self) -> Result<Vec<ClonedRepository>> {
        self.repository_service.cached_repositories()
    }

    /// Existing mirror for a clone URL, when this node has cloned it before.
    pub(crate) fn cached_repository_mirror(&self, clone_url: &str) -> Result<Option<PathBuf>> {
        self.repository_service.cached_mirror(clone_url)
    }

    /// Destination mirror path for a clone URL, whether or not it exists yet.
    pub(crate) fn repository_mirror_path(&self, clone_url: &str) -> Result<PathBuf> {
        self.repository_service.mirror_path(clone_url)
    }

    /// When the mirror of `clone_url` was last known to match its remote, when
    /// this node has recorded a refresh for it.
    pub(crate) fn mirror_refresh(&self, clone_url: &str) -> Result<Option<CachedMirrorRefresh>> {
        self.repository_service.mirror_refresh(clone_url)
    }

    /// Records when the mirror of `refresh.clone_url` was last known to match
    /// its remote.
    pub(crate) fn record_mirror_refresh(&self, refresh: &CachedMirrorRefresh) -> Result<()> {
        self.repository_service.record_mirror_refresh(refresh)
    }

    /// Record a cached clone so later sessions can reuse it.
    pub(crate) fn register_cloned_repository(&self, repository: &ClonedRepository) -> Result<()> {
        self.repository_service
            .register_cloned_repository(repository)
    }

    pub(crate) fn session_task_supervisors(
        &self,
    ) -> Result<MutexGuard<'_, BTreeMap<AgentSessionId, TaskSupervisor>>> {
        self.process_service.task_supervisors()
    }

    pub(crate) fn session_terminals(
        &self,
    ) -> Result<MutexGuard<'_, BTreeMap<loom_core::TerminalId, AgentSessionId>>> {
        self.process_service.session_terminals()
    }

    pub(crate) fn runs(
        &self,
    ) -> Result<MutexGuard<'_, BTreeMap<loom_core::RunId, Arc<RunHandle>>>> {
        self.run_service.runs()
    }

    pub(crate) fn persisted_runs(
        &self,
    ) -> Result<MutexGuard<'_, BTreeMap<loom_core::RunId, PersistedRunSummary>>> {
        self.run_service.persisted_runs()
    }

    pub(crate) fn journal(&self) -> Result<MutexGuard<'_, EventJournal>> {
        self.journal.lock().map_err(|_| {
            LoomError::new(ErrorCode::Internal, "event journal lock was poisoned", true)
        })
    }

    pub(crate) fn session_policies(
        &self,
    ) -> Result<MutexGuard<'_, BTreeMap<AgentSessionId, ApprovalPolicy>>> {
        self.session_service.policies()
    }

    pub(crate) fn auto_approve_actions(
        &self,
    ) -> Result<MutexGuard<'_, BTreeMap<AgentSessionId, bool>>> {
        self.session_service.auto_approve_actions()
    }

    pub(crate) fn workspace_configs(
        &self,
    ) -> Result<MutexGuard<'_, BTreeMap<WorkspaceId, WorkspaceConfig>>> {
        self.workspace_service.configs()
    }

    pub(crate) fn resource_monitor(&self) -> Result<MutexGuard<'_, ResourceMonitor>> {
        self.resource_monitor.lock().map_err(|_| {
            LoomError::new(
                ErrorCode::Internal,
                "worker resource monitor lock was poisoned",
                true,
            )
        })
    }

    pub fn set_event_retention(&self, limit: usize) -> Result<()> {
        if limit == 0 {
            return Err(LoomError::invalid_request(
                "event retention limit must be greater than zero",
            ));
        }
        self.journal()?.set_retention(limit);
        Ok(())
    }

    pub fn event_retention(&self) -> Result<usize> {
        Ok(self.journal()?.retention_limit.max(1))
    }

    /// Archive-retention policy this backend applies to archived sessions.
    ///
    /// The sweep reads this at the start of every round, so a policy change
    /// applies from the next round instead of starting one.
    pub fn archive_retention(&self) -> loom_protocol::ArchiveRetentionPolicy {
        *self
            .archive_retention
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// Whether the policy in effect is the one stored in durable state, either
    /// loaded when this backend opened or written by a later change.
    pub fn archive_retention_is_stored(&self) -> bool {
        self.archive_retention_stored.load(Ordering::SeqCst)
    }

    /// Loads the policy stored in durable state, when this backend has a durable
    /// database that holds one.
    ///
    /// The backend installs no policy by itself, so a start without an explicit
    /// flag applies exactly what the previous start or a client stored.
    pub(crate) fn restore_archive_retention(&self) -> Result<()> {
        let Some(persistence) = &self.persistence else {
            return Ok(());
        };
        let Some(stored) = persistence.load_archive_retention_policy()? else {
            return Ok(());
        };
        self.store_archive_retention(loom_protocol::ArchiveRetentionPolicy {
            retention_ms: stored.retention_ms.filter(|retention_ms| *retention_ms > 0),
            force_discard_worktrees: stored.force_discard_worktrees,
            sub_agent_retention_ms: stored.sub_agent_retention_ms.filter(|value| *value > 0),
        });
        // Recorded so a start without explicit input can log that the stored
        // policy won rather than the built-in default.
        self.archive_retention_stored.store(true, Ordering::SeqCst);
        log::info!(
            "[loom-server] archive retention policy loaded from durable state: {}",
            crate::retention::describe_archive_retention(self.archive_retention())
        );
        Ok(())
    }

    /// Sets the archive-retention policy of this backend without writing durable
    /// state, and reports the policy that was in effect.
    fn store_archive_retention(
        &self,
        policy: loom_protocol::ArchiveRetentionPolicy,
    ) -> loom_protocol::ArchiveRetentionPolicy {
        let normalized = loom_protocol::ArchiveRetentionPolicy {
            retention_ms: policy.retention_ms.filter(|retention_ms| *retention_ms > 0),
            force_discard_worktrees: policy.force_discard_worktrees,
            sub_agent_retention_ms: policy.sub_agent_retention_ms.filter(|value| *value > 0),
        };
        let mut current = self
            .archive_retention
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        std::mem::replace(&mut *current, normalized)
    }

    /// Sets the archive-retention policy and writes it to durable state.
    ///
    /// A zero retention window is normalized to `None`, which means retention
    /// is disabled. The sweep reads the policy at the start of every round, so
    /// the change applies from the next sweep; it does not start one. A failed
    /// durable write restores the previous in-memory policy and fails the
    /// request, so the sweep and the stored policy never disagree.
    pub fn set_archive_retention(
        &self,
        policy: loom_protocol::ArchiveRetentionPolicy,
    ) -> Result<()> {
        let previous = self.store_archive_retention(policy);
        let normalized = self.archive_retention();
        if let Err(error) = self.persist_state() {
            self.store_archive_retention(previous);
            log::error!(
                "[loom-server] archive retention policy was not changed; durable state save failed: {}",
                error.message
            );
            return Err(error);
        }
        if self.persistence.is_some() {
            self.archive_retention_stored.store(true, Ordering::SeqCst);
        }
        if previous != normalized {
            log::info!(
                "[loom-server] archive retention policy changed: {} -> {}; the change applies from the next sweep",
                crate::retention::describe_archive_retention(previous),
                crate::retention::describe_archive_retention(normalized)
            );
        }
        Ok(())
    }
}

impl InProcessBackend {
    /// Root of the per-session build-output cache, absent for an in-memory
    /// backend.
    pub(crate) fn build_cache_root(&self) -> Option<PathBuf> {
        self.build_cache_base.clone()
    }

    /// The Cargo target directory that belongs to one session.
    pub(crate) fn build_cache_for_session(&self, session_id: AgentSessionId) -> Option<PathBuf> {
        self.build_cache_base
            .as_ref()
            .map(|base| base.join(session_id.to_string()))
    }

    /// Removes a session's build cache. Best effort: build output is
    /// reproducible, so a failure is logged instead of failing the caller.
    pub(crate) fn remove_build_cache(&self, session_id: AgentSessionId) {
        let Some(path) = self.build_cache_for_session(session_id) else {
            return;
        };
        match fs::remove_dir_all(&path) {
            Ok(()) => {
                log::info!("[loom-server] removed the build cache of session {session_id}");
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => log::warn!(
                "[loom-server] could not remove the build cache of session {session_id}: {error}"
            ),
        }
    }

    /// Removes caches left behind by sessions that no longer exist, and by
    /// archived sessions (which no longer run), so a crash between the state
    /// change and the cache removal cannot leak build output forever.
    pub(crate) fn sweep_orphan_build_caches(&self) -> Result<()> {
        let Some(base) = &self.build_cache_base else {
            return Ok(());
        };
        let known = {
            let sessions = self.sessions()?;
            sessions
                .list_in_workspace(None, true)
                .into_iter()
                .map(|session| (session.id.to_string(), session.state))
                .collect::<BTreeMap<_, _>>()
        };
        let entries = match fs::read_dir(base) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => {
                return Err(LoomError::new(
                    ErrorCode::Internal,
                    format!("could not read the session build cache directory: {error}"),
                    true,
                ));
            }
        };
        let mut removed = 0_usize;
        for entry in entries.flatten() {
            let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            let remove = match known.get(&name) {
                None => true,
                Some(state) => *state == AgentSessionState::Archived,
            };
            if !remove {
                continue;
            }
            match fs::remove_dir_all(entry.path()) {
                Ok(()) => removed += 1,
                Err(error) => log::warn!(
                    "[loom-server] could not remove orphaned session build cache '{}': {error}",
                    entry.path().display()
                ),
            }
        }
        if removed > 0 {
            log::info!("[loom-server] removed {removed} orphaned session build cache(s)");
        }
        Ok(())
    }
}
