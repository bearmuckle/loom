use super::*;

impl InProcessBackend {
    pub fn connect(self: &Arc<Self>) -> InProcessConnection {
        InProcessConnection {
            backend: Arc::clone(self),
            negotiated_capabilities: Arc::new(Mutex::new(None)),
            auth: None,
            max_response_bytes: None,
        }
    }

    pub fn connect_authenticated(self: &Arc<Self>, auth: AuthSession) -> InProcessConnection {
        InProcessConnection {
            backend: Arc::clone(self),
            negotiated_capabilities: Arc::new(Mutex::new(None)),
            auth: Some(auth),
            max_response_bytes: None,
        }
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
